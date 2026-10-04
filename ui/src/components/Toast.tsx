import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from "react";
import { X } from "lucide-react";
import { Button } from "./Button";

type Notice = { id: number; tone: "success" | "error"; message: string };
type Notify = (tone: Notice["tone"], message: string) => void;
const toastContext = createContext<Notify | null>(null);

export function Toast({ notices, dismiss }: { notices: readonly Notice[]; dismiss: (id: number) => void }) {
  return (
    <div className="ui-toast-stack" aria-label="Notifications">
      {notices.map((notice) => (
        <div key={notice.id} className="ui-toast" role={notice.tone === "error" ? "alert" : "status"}>
          <strong className={`ui-toast__${notice.tone}`}>{notice.tone === "error" ? "Error" : "Success"}</strong>
          <span>{notice.message}</span>
          <Button iconOnly variant="ghost" aria-label="Dismiss notification" onClick={() => dismiss(notice.id)}>
            <X size={16} aria-hidden="true" />
          </Button>
        </div>
      ))}
    </div>
  );
}

export function ToastProvider({ children }: { children: ReactNode }) {
  const [notices, setNotices] = useState<Notice[]>([]);
  const nextId = useRef(0);
  const timers = useRef<number[]>([]);
  useEffect(() => () => { for (const timer of timers.current) window.clearTimeout(timer); }, []);
  const dismiss = useCallback((id: number) => setNotices((items) => items.filter((item) => item.id !== id)), []);
  const notify = useCallback<Notify>((tone, message) => {
    const id = ++nextId.current;
    setNotices((items) => [...items, { id, tone, message }]);
    timers.current.push(window.setTimeout(() => dismiss(id), 5000));
  }, [dismiss]);
  return <toastContext.Provider value={notify}>{children}<Toast notices={notices} dismiss={dismiss} /></toastContext.Provider>;
}

export function useToast(): Notify {
  const notify = useContext(toastContext);
  if (!notify) throw new Error("ToastProvider is missing.");
  return notify;
}
