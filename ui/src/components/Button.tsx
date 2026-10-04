import type { ButtonHTMLAttributes } from "react";
import { clsx } from "clsx";

type ButtonProps = ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: "primary" | "default" | "ghost" | "destructive";
  size?: "sm" | "default" | "lg";
} & ({ iconOnly?: false } | { iconOnly: true; "aria-label": string });

export function Button({ variant = "default", size = "default", iconOnly = false, className, type = "button", ...props }: ButtonProps) {
  return <button type={type} className={clsx("ui-button", `ui-button--${variant}`, `ui-button--${size}`, iconOnly && "ui-button--icon", className)} {...props} />;
}
