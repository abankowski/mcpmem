import { useEffect, useId, useRef, type ReactNode } from "react";
import { X } from "lucide-react";
import { Button } from "./Button";

interface SheetProps {
  open: boolean;
  title: string;
  children: ReactNode;
  footer?: ReactNode;
  onClose: () => void;
}

export function Sheet({ open, title, children, footer, onClose }: SheetProps) {
  const dialog = useRef<HTMLDialogElement>(null);
  const previousFocus = useRef<HTMLElement | null>(null);
  const titleId = useId();
  useEffect(() => {
    const element = dialog.current;
    if (!element) return;
    if (open && !element.open) {
      previousFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      element.showModal();
      element.querySelector<HTMLElement>("button, input, select, textarea")?.focus();
    } else if (!open && element.open) {
      element.close();
      previousFocus.current?.focus();
    }
    return () => {
      if (element.open) element.close();
      previousFocus.current?.focus();
    };
  }, [open]);
  return (
    <dialog ref={dialog} className="ui-sheet" aria-labelledby={titleId}
      onCancel={(event) => { event.preventDefault(); onClose(); }}
      onClick={(event) => { if (event.target === event.currentTarget) onClose(); }}>
      <div className="ui-sheet__panel">
        <header className="ui-sheet__header">
          <h2 id={titleId}>{title}</h2>
          <Button iconOnly aria-label="Close panel" variant="ghost" onClick={onClose}><X size={18} aria-hidden="true" /></Button>
        </header>
        <div className="ui-sheet__body">{children}</div>
        {footer && <footer className="ui-sheet__footer">{footer}</footer>}
      </div>
    </dialog>
  );
}
