import { useEffect, useId, useRef, useState } from "react";
import { ChevronLeft, ChevronRight, X } from "lucide-react";
import { api, ApiError } from "../../lib/api";
import type { Attachment } from "../../lib/schemas";
import { formatBytes } from "../../lib/format";
import { Button } from "../../components/Button";
import { Tag } from "../../components/Tag";
import { fileKind } from "./dropzone";
import { STATUS_TONE, labelFor, pageCountLabel } from "./rows";

const PAGE_CHARS = 4096;

export interface LightboxProps {
  workspaceId: string;
  attachment: Attachment;
  /** Open a text file at this page (search hits carry the page). */
  initialPage?: number;
  onClose: () => void;
  onDownload?: (row: Attachment) => void;
}

export function Lightbox({ workspaceId, attachment, initialPage, onClose, onDownload }: LightboxProps) {
  const dialog = useRef<HTMLDialogElement>(null);
  const previousFocus = useRef<HTMLElement | null>(null);
  const titleId = useId();
  const kind = fileKind(attachment.mime);
  const ready = attachment.status === "ready";

  const [page, setPage] = useState(Math.min(Math.max(Math.trunc(initialPage ?? 1), 1), Math.max(attachment.pageCount ?? 1, 1)));
  const [spans, setSpans] = useState<string[]>([]);
  const [nextOffset, setNextOffset] = useState(0);
  const [eof, setEof] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<ApiError | null>(null);
  const [pdfUrl, setPdfUrl] = useState<string | null>(null);
  const moreController = useRef<AbortController | null>(null);

  useEffect(() => {
    const element = dialog.current;
    if (!element) return;
    previousFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    element.showModal();
    element.querySelector<HTMLElement>("button")?.focus();
    return () => {
      if (element.open) element.close();
      previousFocus.current?.focus();
    };
  }, []);

  // Text preview: the server returns one 4096-char span per call. An offset
  // walk appends spans until the page ends; page navigation stays within
  // pageCount. React escapes the page text, so the render is text, not HTML.
  useEffect(() => {
    if (kind !== "text" || !ready) return;
    const controller = new AbortController();
    let active = true;
    moreController.current?.abort();
    moreController.current = null;
    setError(null);
    setSpans([]);
    setNextOffset(0);
    setEof(false);
    if (attachment.pageCount === 0) {
      setBusy(false);
      setEof(true);
      return;
    }
    setBusy(true);
    api.attachmentPage(workspaceId, attachment.attachmentId, page, 0, PAGE_CHARS, controller.signal)
      .then((result) => {
        if (!active) return;
        setSpans([result.text]);
        setNextOffset(result.nextOffset);
        setEof(result.eof);
      })
      .catch((cause: unknown) => {
        if (!active || controller.signal.aborted) return;
        setError(cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The page preview could not be loaded."));
      })
      .finally(() => {
        if (active) setBusy(false);
      });
    return () => {
      active = false;
      controller.abort();
      moreController.current?.abort();
      moreController.current = null;
    };
  }, [workspaceId, attachment.attachmentId, attachment.pageCount, kind, page, ready]);

  // The URL belongs to this effect. Its cleanup always revokes the URL.
  useEffect(() => {
    if (kind !== "pdf") return;
    const controller = new AbortController();
    let active = true;
    let objectUrl: string | null = null;
    setPdfUrl(null);
    setError(null);
    setBusy(true);
    api.attachmentBytes(workspaceId, attachment.attachmentId, controller.signal)
      .then((blob) => {
        if (!active) return;
        objectUrl = URL.createObjectURL(blob);
        setPdfUrl(objectUrl);
      })
      .catch((cause: unknown) => {
        if (!active || controller.signal.aborted) return;
        setError(cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The PDF preview could not be loaded."));
      })
      .finally(() => {
        if (active) setBusy(false);
      });
    return () => {
      active = false;
      controller.abort();
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [workspaceId, attachment.attachmentId, kind]);

  async function showMore(): Promise<void> {
    const controller = new AbortController();
    moreController.current = controller;
    setBusy(true);
    setError(null);
    try {
      const result = await api.attachmentPage(workspaceId, attachment.attachmentId, page, nextOffset, PAGE_CHARS, controller.signal);
      if (controller.signal.aborted) return;
      setSpans((previous) => [...previous, result.text]);
      setNextOffset(result.nextOffset);
      setEof(result.eof);
    } catch (cause: unknown) {
      if (!controller.signal.aborted) {
        setError(cause instanceof ApiError ? cause : new ApiError(0, "network_error", "More page text could not be loaded."));
      }
    } finally {
      if (moreController.current === controller) {
        moreController.current = null;
        setBusy(false);
      }
    }
  }

  const pageCount = attachment.pageCount;
  const statusNote =
    attachment.status === "ready"
      ? null
      : attachment.status === "error"
        ? "Extraction failed. The stored bytes stay available for download."
        : attachment.status === "uploaded"
          ? "Extraction has not started."
          : "Extraction is in progress.";

  return (
    <dialog
      ref={dialog}
      className="ui-files-lightbox"
      aria-labelledby={titleId}
      onCancel={(event) => { event.preventDefault(); onClose(); }}
      onClick={(event) => { if (event.target === event.currentTarget) onClose(); }}
    >
      <header className="ui-files-lightbox__header">
        <div className="ui-files-lightbox__title">
          <h2 id={titleId} title={attachment.filename}>{attachment.filename}</h2>
          <div className="ui-files-row__meta ui-files-mono">
            <span>{labelFor(fileKind(attachment.mime), attachment.filename)} · {formatBytes(attachment.sizeBytes)} ·</span>
            <Tag tone={STATUS_TONE[attachment.status]}>{attachment.status}</Tag>
            {pageCount != null && (attachment.status === "ready" || attachment.status === "error") && (
              <span>· {pageCountLabel(pageCount)}</span>
            )}
          </div>
        </div>
        <Button iconOnly variant="ghost" aria-label="Close preview" onClick={onClose}>
          <X size={18} aria-hidden="true" />
        </Button>
      </header>

      <div className="ui-files-lightbox__body" aria-busy={busy}>
        {kind === "pdf" && pdfUrl && (
          <iframe className="ui-files-lightbox__pdf" src={pdfUrl} title={`${attachment.filename} preview`} />
        )}
        {kind === "pdf" && !pdfUrl && !error && (
          <p className="ui-files-lightbox__note">Loading the PDF…</p>
        )}

        {kind === "text" && ready && (
          <>
            <div className="ui-files-lightbox__toolbar">
              <Button size="sm" variant="ghost" disabled={busy || page <= 1} onClick={() => setPage((previous) => previous - 1)}>
                <ChevronLeft size={14} aria-hidden="true" />Prev
              </Button>
              <span className="ui-files-mono ui-files-mono--muted">
                Page {page}{pageCount != null && pageCount > 0 ? ` of ${pageCount}` : ""}
              </span>
              <Button
                size="sm"
                variant="ghost"
                disabled={busy || pageCount == null || page >= pageCount}
                onClick={() => setPage((previous) => previous + 1)}
              >
                Next<ChevronRight size={14} aria-hidden="true" />
              </Button>
            </div>
            {busy && spans.length === 0 ? (
              <p className="ui-files-lightbox__note">Loading the page…</p>
            ) : spans.length > 0 ? (
              <pre className="ui-files-lightbox__text">{spans}</pre>
            ) : null}
            {!eof && spans.length > 0 && (
              <div className="ui-files-lightbox__more">
                <Button size="sm" variant="ghost" disabled={busy} onClick={() => void showMore()}>
                  Show more text
                </Button>
              </div>
            )}
            {spans.length === 0 && !busy && !error && (
              <p className="ui-files-lightbox__note">This file has no extracted text.</p>
            )}
          </>
        )}

        {kind === "other" && <p className="ui-files-lightbox__note">This file type has no preview.</p>}
        {statusNote && <p className="ui-files-lightbox__note">{statusNote}</p>}
        {error && <p className="ui-files-error ui-files-lightbox__note" role="alert">{error.code}: {error.message}</p>}
      </div>

      {onDownload && (
        <footer className="ui-files-lightbox__footer">
          <Button size="sm" onClick={() => onDownload(attachment)}>Download</Button>
        </footer>
      )}
    </dialog>
  );
}