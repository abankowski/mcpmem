import { useEffect, useRef, useState } from "react";
import { api, ApiError } from "../../lib/api";
import { canAuthorize, requestConsent } from "../../lib/auth";
import { formatBytes } from "../../lib/format";
import { useToast } from "../../components/Toast";
import { ConfirmDialog } from "../../components/ConfirmDialog";
import { Button } from "../../components/Button";
import type { Attachment } from "../../lib/schemas";
import { FileDropzone, fileKind, mimeForFile } from "./dropzone";
import { FileRow, labelFor } from "./rows";
import { Lightbox } from "./lightbox";
import "./files.css";

const POLL_INTERVAL_MS = 2000;
const LIST_LIMIT = 1000;
const EMPTY_ROWS: readonly Attachment[] = [];

export interface FilesPanelProps {
  workspaceId: string;
  entityName: string;
  /** Hide upload and delete unless the selected workspace allows writes. */
  canWrite: boolean;
  /** Report an attachment API failure to an enclosing page. */
  onApiError?: (error: ApiError) => void;
  /** Report the row count for a tab badge. */
  onCountChange?: (count: number) => void;
}

function formatError(error: ApiError): string {
  return `${error.code}: ${error.message}`;
}

export function FilesPanel({ workspaceId, entityName, canWrite, onApiError, onCountChange }: FilesPanelProps) {
  const notify = useToast();
  const [rows, setRows] = useState<readonly Attachment[]>([]);
  const [loadedFor, setLoadedFor] = useState<{ workspaceId: string; entityName: string } | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<ApiError | null>(null);
  const [pollError, setPollError] = useState<ApiError | null>(null);
  const [reloadKey, setReloadKey] = useState(0);
  const [busyFile, setBusyFile] = useState<File | null>(null);
  const [busyFor, setBusyFor] = useState<{ workspaceId: string; entityName: string } | null>(null);
  const [queuedCount, setQueuedCount] = useState(0);
  const [failedUpload, setFailedUpload] = useState<{ file: File; error: ApiError } | null>(null);
  const [previewId, setPreviewId] = useState<number | null>(null);
  const [confirmDelete, setConfirmDelete] = useState<Attachment | null>(null);
  const [deletingId, setDeletingId] = useState<number | null>(null);

  const queue = useRef<File[]>([]);
  const running = useRef(false);
  const uploadController = useRef<AbortController | null>(null);
  const downloadControllers = useRef(new Set<AbortController>());
  const deleteController = useRef<AbortController | null>(null);
  const currentScope = useRef({ workspaceId, entityName });
  currentScope.current.workspaceId = workspaceId;
  currentScope.current.entityName = entityName;
  const apiErrorHandler = useRef(onApiError);
  const countHandler = useRef(onCountChange);
  useEffect(() => { apiErrorHandler.current = onApiError; });
  useEffect(() => { countHandler.current = onCountChange; });

  function isCurrent(): boolean {
    return currentScope.current.workspaceId === workspaceId && currentScope.current.entityName === entityName;
  }

  useEffect(() => () => {
    queue.current.length = 0;
    uploadController.current?.abort();
    uploadController.current = null;
    for (const controller of downloadControllers.current) controller.abort();
    downloadControllers.current.clear();
    deleteController.current?.abort();
    deleteController.current = null;
    running.current = false;
  }, [workspaceId, entityName]);

  // Load the entity's rows. Clear stale rows immediately so a previous
  // node's files never flash on a switch; the abort cancels in-flight reads.
  useEffect(() => {
    const controller = new AbortController();
    let active = true;
    setLoading(true);
    setLoadError(null);
    setPollError(null);
    setLoadedFor(null);
    setRows([]);
    setBusyFile(null);
    setBusyFor(null);
    setQueuedCount(0);
    setPreviewId(null);
    setConfirmDelete(null);
    setFailedUpload(null);
    api.attachments(workspaceId, entityName, LIST_LIMIT, controller.signal)
      .then((result) => {
        if (!active || !isCurrent()) return;
        setRows(result.attachments);
        setLoadedFor({ workspaceId, entityName });
      })
      .catch((cause: unknown) => {
        if (!active || !isCurrent() || controller.signal.aborted) return;
        const error = cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The attachment list could not be loaded.");
        setLoadError(error);
        apiErrorHandler.current?.(error);
      })
      .finally(() => {
        if (active && isCurrent()) setLoading(false);
      });
    return () => { active = false; controller.abort(); };
  }, [workspaceId, entityName, reloadKey]);

  // Poll active rows until each row reaches ready or error. An error in one
  // request does not stop later polls or discard the other rows.
  const pageReady = loadedFor?.workspaceId === workspaceId && loadedFor.entityName === entityName;
  const visibleRows = pageReady ? rows : EMPTY_ROWS;
  const activeIds = visibleRows
    .filter((row) => row.status === "uploaded" || row.status === "extracting")
    .map((row) => row.attachmentId)
    .join(",");
  useEffect(() => {
    if (!activeIds) return;
    const controller = new AbortController();
    let cancelled = false;
    let inFlight = false;
    async function tick(): Promise<void> {
      if (inFlight) return;
      inFlight = true;
      try {
        // One list read per tick reconciles every active row for this entity.
        // A per-id fan-out would fire the whole active set at once: a node
        // with a thousand queued extractions would burst a thousand requests
        // every interval. The list has the same 1000-row cap the rows use.
        let rows: Attachment[] = [];
        let failure: ApiError | null = null;
        try {
          const response = await api.attachments(workspaceId, entityName, LIST_LIMIT, controller.signal);
          rows = response.attachments;
        } catch (cause) {
          if (cancelled || !isCurrent() || controller.signal.aborted) return;
          failure = cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The file status could not be loaded.");
        }
        const byId = new Map<number, Attachment>();
        for (const row of rows) byId.set(row.attachmentId, row);
        if (failure == null && byId.size > 0) {
          setRows((previous) => {
            let changed = false;
            const next = previous.map((row) => {
              const update = byId.get(row.attachmentId);
              if (!update) return row;
              if (
                update.status !== row.status || update.errorStage !== row.errorStage ||
                update.lastError !== row.lastError || update.pageCount !== row.pageCount || update.revision !== row.revision
              ) changed = true;
              return update;
            });
            return changed ? next : previous;
          });
        }
        if (!cancelled && isCurrent()) {
          setPollError((previous) =>
            previous?.status === failure?.status && previous?.code === failure?.code &&
            previous?.message === failure?.message ? previous : failure,
          );
        }
      } finally {
        inFlight = false;
      }
    }
    void tick();
    const timer = window.setInterval(() => void tick(), POLL_INTERVAL_MS);
    return () => { cancelled = true; window.clearInterval(timer); controller.abort(); };
  }, [workspaceId, entityName, activeIds]);

  useEffect(() => { countHandler.current?.(visibleRows.length); }, [visibleRows]);

  // A deleted row must not keep its preview open.
  useEffect(() => {
    if (previewId != null && !visibleRows.some((row) => row.attachmentId === previewId)) setPreviewId(null);
  }, [visibleRows, previewId]);

  async function refreshRow(attachmentId: number, signal: AbortSignal): Promise<void> {
    try {
      const detail = await api.attachment(workspaceId, attachmentId, signal);
      if (!isCurrent() || signal.aborted) return;
      setRows((previous) => [detail, ...previous.filter((row) => row.attachmentId !== detail.attachmentId)]);
    } catch {
      if (!isCurrent() || signal.aborted) return;
      // The upload succeeded. Read the list if the detail request failed.
      try {
        const result = await api.attachments(workspaceId, entityName, LIST_LIMIT, signal);
        if (isCurrent() && !signal.aborted) setRows(result.attachments);
      } catch (cause) {
        if (!isCurrent() || signal.aborted) return;
        const error = cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The attachment list could not be refreshed.");
        setLoadError(error);
        apiErrorHandler.current?.(error);
      }
    }
  }

  function enqueue(files: readonly File[]): void {
    for (const file of files) queue.current.push(file);
    setQueuedCount(queue.current.length);
    if (!running.current) void runQueue();
  }

  function retryUpload(): void {
    if (!failedUpload) return;
    queue.current.unshift(failedUpload.file);
    setQueuedCount(queue.current.length);
    setFailedUpload(null);
    if (!running.current) void runQueue();
  }

  function dismissFailedUpload(): void {
    setFailedUpload(null);
    if (queue.current.length > 0 && !running.current) void runQueue();
  }

  async function runQueue(): Promise<void> {
    running.current = true;
    try {
      while (isCurrent() && queue.current.length > 0) {
        const file = queue.current.shift();
        if (!file) break;
        setQueuedCount(queue.current.length);
        const controller = new AbortController();
        uploadController.current = controller;
        setBusyFile(file);
        setBusyFor({ workspaceId, entityName });
        try {
          const result = await api.uploadAttachment({
            workspaceId, entityName, filename: file.name, mime: mimeForFile(file), content: file,
          }, controller.signal);
          if (!isCurrent() || controller.signal.aborted) return;
          await refreshRow(result.attachmentId, controller.signal);
        } catch (cause) {
          if (!isCurrent() || controller.signal.aborted) return;
          const error = cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The upload could not be completed.");
          setFailedUpload({ file, error });
          apiErrorHandler.current?.(error);
          return;
        } finally {
          if (uploadController.current === controller) uploadController.current = null;
          if (isCurrent()) setBusyFile(null);
        }
      }
    } finally {
      if (isCurrent()) running.current = false;
    }
  }

  async function downloadRow(row: Attachment): Promise<void> {
    const controller = new AbortController();
    downloadControllers.current.add(controller);
    try {
      const blob = await api.attachmentBytes(workspaceId, row.attachmentId, controller.signal);
      if (!isCurrent() || controller.signal.aborted) return;
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = row.filename;
      document.body.appendChild(anchor);
      try {
        anchor.click();
      } finally {
        anchor.remove();
        window.setTimeout(() => URL.revokeObjectURL(url), 1000);
      }
    } catch (cause) {
      if (!isCurrent() || controller.signal.aborted) return;
      const error = cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The file could not be downloaded.");
      notify("error", formatError(error));
      apiErrorHandler.current?.(error);
    } finally {
      downloadControllers.current.delete(controller);
    }
  }

  async function deleteRow(): Promise<void> {
    if (!confirmDelete) return;
    const id = confirmDelete.attachmentId;
    const controller = new AbortController();
    deleteController.current = controller;
    setDeletingId(id);
    try {
      await api.deleteAttachment(workspaceId, id, controller.signal);
      if (!isCurrent() || controller.signal.aborted) return;
      setRows((previous) => previous.filter((row) => row.attachmentId !== id));
      setConfirmDelete(null);
    } catch (cause) {
      if (!isCurrent() || controller.signal.aborted) return;
      const error = cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The file could not be deleted.");
      apiErrorHandler.current?.(error);
      throw new Error(formatError(error));
    } finally {
      if (deleteController.current === controller) deleteController.current = null;
      if (isCurrent()) setDeletingId(null);
    }
  }

  const visibleBusy = busyFor?.workspaceId === workspaceId && busyFor.entityName === entityName ? busyFile : null;
  const preview = previewId != null ? visibleRows.find((row) => row.attachmentId === previewId) ?? null : null;
  const pendingLabel = visibleBusy ? labelFor(fileKind(mimeForFile(visibleBusy)), visibleBusy.name) : null;
  const showEmpty = !loading && !loadError && pageReady && visibleRows.length === 0 && !visibleBusy;

  return (
    <section className="ui-files" aria-label={`Files of ${entityName}`}>
      {canWrite && <FileDropzone disabled={!pageReady || visibleBusy != null || failedUpload != null} onFiles={enqueue} />}
      {pageReady && queuedCount > 0 && <p className="ui-files-state" role="status">{queuedCount} files waiting to upload.</p>}
      {pageReady && failedUpload && (
        <div className="ui-files-error" role="alert">
          {failedUpload.file.name}: {formatError(failedUpload.error)}
          <Button size="sm" variant="ghost" className="ui-files-error__retry" onClick={retryUpload}>Retry</Button>
          <Button size="sm" variant="ghost" onClick={dismissFailedUpload}>Dismiss</Button>
        </div>
      )}
      {loadError && (
        <div className="ui-files-state" role="alert">
          <span className="ui-files-error">{formatError(loadError)}</span>
          {loadError.status === 403 && loadError.code === "insufficient_scope" && canAuthorize("graph") && (
            <Button size="sm" variant="ghost" onClick={() => { void requestConsent(["attachments"]); }}>
              Grant attachments
            </Button>
          )}
          <Button size="sm" variant="ghost" className="ui-files-error__retry" onClick={() => setReloadKey((key) => key + 1)}>
            Retry
          </Button>
        </div>
      )}
      {pageReady && pollError && <p className="ui-files-error" role="alert">{formatError(pollError)}</p>}
      <ul className="ui-files-list">
        {visibleBusy && (
          <li className="ui-files-row">
            <div className="ui-files-thumb" aria-hidden="true">
              <span className="ui-files-thumb__label">{pendingLabel}</span>
            </div>
            <div className="ui-files-row__main">
              <div className="ui-files-row__name" title={visibleBusy.name}>{visibleBusy.name}</div>
              <div className="ui-files-row__meta ui-files-mono">
                <span>{pendingLabel} · {formatBytes(visibleBusy.size)} · Upload in progress</span>
              </div>
              <div className="ui-files-progress" role="progressbar" aria-label={`Uploading ${visibleBusy.name}`} />
            </div>
          </li>
        )}
        {visibleRows.map((row) => (
          <FileRow
            key={row.attachmentId}
            row={row}
            canWrite={canWrite}
            deleting={deletingId === row.attachmentId}
            onPreview={(row) => { setPreviewId(row.attachmentId); }}
            onDownload={downloadRow}
            onDelete={setConfirmDelete}
          />
        ))}
      </ul>
      {(loading || (!pageReady && !loadError)) && <p className="ui-files-state">Loading files…</p>}
      {showEmpty && <p className="ui-files-state">No files attached yet.</p>}
      {preview && (
        <Lightbox
          workspaceId={workspaceId}
          attachment={preview}
          onClose={() => setPreviewId(null)}
          onDownload={downloadRow}
        />
      )}
      <ConfirmDialog
        open={pageReady && confirmDelete != null}
        title="Delete file"
        confirmLabel="Delete"
        onConfirm={deleteRow}
        onClose={() => setConfirmDelete(null)}
      >
        {confirmDelete ? <>Delete <strong>{confirmDelete.filename}</strong>? The stored bytes and extracted text are removed. This cannot be undone.</> : null}
      </ConfirmDialog>
    </section>
  );
}