import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { Button } from "./Button";

interface ConfirmDialogProps {
  open: boolean;
  title: string;
  children: ReactNode;
  confirmLabel: string;
  onConfirm: () => Promise<void> | void;
  onClose: () => void;
}

export function ConfirmDialog({ open, title, children, confirmLabel, onConfirm, onClose }: ConfirmDialogProps) {
  const dialog = useRef<HTMLDialogElement>(null);
  const previousFocus = useRef<HTMLElement | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const titleId = useId();
  const descriptionId = useId();
  useEffect(() => {
    const element = dialog.current;
    if (!element) return;
    if (open && !element.open) {
      previousFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      element.showModal();
      element.querySelector<HTMLElement>("button")?.focus();
    } else if (!open && element.open) {
      element.close();
      previousFocus.current?.focus();
    }
    return () => {
      if (element.open) element.close();
      previousFocus.current?.focus();
    };
  }, [open]);
  async function confirm(): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      await onConfirm();
      onClose();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "The action failed. Try again.");
    } finally {
      setBusy(false);
    }
  }
  return (
    <dialog ref={dialog} className="ui-confirm" aria-labelledby={titleId} aria-describedby={descriptionId}
      onCancel={(event) => { event.preventDefault(); if (!busy) onClose(); }}>
      <h2 id={titleId}>{title}</h2>
      <div id={descriptionId}>{children}</div>
      {error && <p className="ui-confirm__error" role="alert">{error}</p>}
      <div className="ui-confirm__actions">
        <Button onClick={onClose} disabled={busy}>Cancel</Button>
        <Button variant="destructive" onClick={confirm} disabled={busy}>{busy ? "Please wait" : confirmLabel}</Button>
      </div>
    </dialog>
  );
}
