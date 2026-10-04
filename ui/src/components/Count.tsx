import type { HTMLAttributes } from "react";
import { clsx } from "clsx";

export function Count({ className, ...props }: HTMLAttributes<HTMLSpanElement>) {
  return <span className={clsx("ui-count", className)} {...props} />;
}
