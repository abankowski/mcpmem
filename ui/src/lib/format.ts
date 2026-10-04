function isoTimestamp(microseconds: number | null | undefined): string | null {
  if (microseconds == null || !Number.isFinite(microseconds)) return null;
  const date = new Date(Math.trunc(microseconds / 1000));
  return Number.isNaN(date.getTime()) ? null : date.toISOString();
}

export function formatDate(microseconds: number | null | undefined): string {
  return isoTimestamp(microseconds)?.slice(0, 10) ?? "Unknown";
}

export function formatTime(microseconds: number | null | undefined): string {
  const timestamp = isoTimestamp(microseconds);
  return timestamp ? `${timestamp.slice(11, 16)} UTC` : "Unknown";
}

export function formatTimestamp(microseconds: number | null | undefined): string {
  const timestamp = isoTimestamp(microseconds);
  return timestamp ? `${timestamp.slice(0, 10)} ${timestamp.slice(11, 16)} UTC` : "Unknown";
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}
