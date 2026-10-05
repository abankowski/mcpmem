import { useState, type ChangeEvent, type DragEvent } from "react";
import { clsx } from "clsx";
import { Upload } from "lucide-react";

// A browser File often carries an empty MIME type. Map known textual
// extensions to the approved text family; the server remains the authority.
const TEXT_EXTENSIONS: Record<string, true> = {
  txt: true, md: true, markdown: true, csv: true, log: true, json: true,
  yml: true, yaml: true, toml: true, xml: true, html: true, htm: true,
  css: true, js: true, ts: true, rs: true, py: true, go: true, java: true,
  c: true, h: true, cpp: true, hpp: true, rb: true, sh: true, sql: true,
  ini: true, env: true,
};

export type FileKind = "text" | "pdf" | "other";

// The approved upload contract allows the configured text family and PDF
// only. Every consumer (dropzone, rows, lightbox) uses this one decision.
export function fileKind(mime: string): FileKind {
  if (mime === "application/pdf") return "pdf";
  if (mime.startsWith("text/")) return "text";
  return "other";
}

export function mimeForFile(file: File): string {
  if (file.type) return file.type;
  const extension = file.name.split(".").pop()?.toLowerCase() ?? "";
  if (extension === "pdf") return "application/pdf";
  if (TEXT_EXTENSIONS[extension]) return "text/plain";
  return "application/octet-stream";
}

export interface FileDropzoneProps {
  disabled?: boolean;
  onFiles: (files: readonly File[]) => void;
}

export function FileDropzone({ disabled = false, onFiles }: FileDropzoneProps) {
  const [dragging, setDragging] = useState(false);
  const [rejected, setRejected] = useState<string | null>(null);

  function accept(files: FileList | readonly File[]): void {
    const list = Array.from(files);
    const invalid = list.find((file) => fileKind(mimeForFile(file)) === "other");
    if (invalid) {
      setRejected(`${invalid.name}: this file type is not supported.`);
      return;
    }
    if (list.length > 0) {
      setRejected(null);
      onFiles(list);
    }
  }

  function handleChange(event: ChangeEvent<HTMLInputElement>): void {
    if (event.target.files) accept(event.target.files);
    // Clear the value so picking the same file again fires change.
    event.target.value = "";
  }

  function handleDrop(event: DragEvent<HTMLLabelElement>): void {
    event.preventDefault();
    setDragging(false);
    if (!disabled) accept(event.dataTransfer.files);
  }

  return (
    <div className="ui-files-drop">
      <label
        className={clsx("ui-files-dropzone", dragging && "ui-files-dropzone--over", disabled && "ui-files-dropzone--disabled")}
        onDragEnter={(event) => { event.preventDefault(); if (!disabled) setDragging(true); }}
        onDragOver={(event) => { event.preventDefault(); if (!disabled) setDragging(true); }}
        onDragLeave={() => setDragging(false)}
        onDrop={handleDrop}
      >
        <input
          className="ui-visually-hidden"
          type="file"
          multiple
          accept="text/*,application/pdf"
          disabled={disabled}
          onChange={handleChange}
        />
        <Upload size={20} aria-hidden="true" />
        <span className="ui-files-dropzone__text">Drop files here or <span className="ui-files-dropzone__browse">browse</span></span>
        <span className="ui-files-mono ui-files-mono--muted">50 MiB default per-file limit · text and PDF</span>
      </label>
      {rejected && <p className="ui-files-error" role="alert">{rejected}</p>}
      <p className="ui-files-mono ui-files-mono--muted">Other file types coming soon</p>
    </div>
  );
}