import { Trash2 } from "lucide-react";
import type { Attachment } from "../../lib/schemas";
import { formatBytes } from "../../lib/format";
import { Button } from "../../components/Button";
import { Tag } from "../../components/Tag";
import { fileKind, type FileKind } from "./dropzone";

// The status values map exactly to the server values. Only the tone varies.
export const STATUS_TONE: Record<Attachment["status"], "neutral" | "ok" | "warn" | "error"> = {
  uploaded: "neutral",
  extracting: "warn",
  ready: "ok",
  error: "error",
};

// The 64 px thumbnail label: PDF, TXT, or the file extension in caps.
// Rows, the lightbox header, and the pending upload row share this one rule.
export function labelFor(kind: FileKind, filename: string): string {
  if (kind === "pdf") return "PDF";
  if (kind === "text") return "TXT";
  const extension = filename.split(".").pop()?.toUpperCase() ?? "";
  return extension || "FILE";
}

export function pageCountLabel(count: number): string {
  return `${count} ${count === 1 ? "page" : "pages"}`;
}

export interface FileRowProps {
  row: Attachment;
  canWrite: boolean;
  deleting: boolean;
  onPreview: (row: Attachment) => void;
  onDownload: (row: Attachment) => void;
  onDelete: (row: Attachment) => void;
}

export function FileRow({ row, canWrite, deleting, onPreview, onDownload, onDelete }: FileRowProps) {
  const processing = row.status === "uploaded" || row.status === "extracting";
  const label = labelFor(fileKind(row.mime), row.filename);

  return (
    <li className="ui-files-row">
      <div className="ui-files-thumb" aria-hidden="true">
        <span className="ui-files-thumb__label">{label}</span>
      </div>
      <div className="ui-files-row__main">
        <div className="ui-files-row__name" title={row.filename}>{row.filename}</div>
        <div className="ui-files-row__meta ui-files-mono">
          <span>{label} · {formatBytes(row.sizeBytes)} ·</span>
          <Tag tone={STATUS_TONE[row.status]}>{row.status}</Tag>
          {row.pageCount != null && (row.status === "ready" || row.status === "error") && (
            <span>· {pageCountLabel(row.pageCount)}</span>
          )}
          {processing && row.errorStage != null && <Tag tone="error">{row.errorStage}</Tag>}
        </div>
        {row.lastError != null && (
          <p className="ui-files-error">{row.errorStage != null ? `${row.errorStage}: ` : ""}{row.lastError}</p>
        )}
        <div className="ui-files-row__actions">
          <Button size="sm" variant="ghost" disabled={row.status !== "ready"} onClick={() => onPreview(row)}>
            Preview
          </Button>
          <Button size="sm" variant="ghost" onClick={() => onDownload(row)}>
            Download
          </Button>
          {canWrite && (
            <Button size="sm" variant="ghost" iconOnly aria-label={`Delete ${row.filename}`} disabled={deleting} onClick={() => onDelete(row)}>
              <Trash2 size={14} aria-hidden="true" />
            </Button>
          )}
        </div>
        {processing && (
          <div className="ui-files-progress" role="progressbar" aria-label={`Processing ${row.filename}`} />
        )}
      </div>
    </li>
  );
}