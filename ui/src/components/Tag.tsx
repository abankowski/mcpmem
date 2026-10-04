import type { HTMLAttributes } from "react";
import { clsx } from "clsx";

interface TagProps extends HTMLAttributes<HTMLSpanElement> {
  tone?: "neutral" | "ok" | "warn" | "error";
  dotColor?: string;
}

export function Tag({ tone = "neutral", dotColor, children, className, ...props }: TagProps) {
  return (
    <span className={clsx("ui-tag", `ui-tag--${tone}`, className)} {...props}>
      {dotColor && <span className="ui-tag__dot" style={{ backgroundColor: dotColor }} aria-hidden="true" />}
      {children}
    </span>
  );
}
