import { useEffect, useState } from "react";
import { z } from "zod";
import { CircleAlert } from "lucide-react";
import { api, ApiError } from "../../lib/api";
import { vectorStatsSchema } from "../../lib/schemas";
import { Button } from "../../components/Button";
import { formatError, type AdminPaneProps } from "./page";

type VectorStats = z.infer<typeof vectorStatsSchema>;

/**
 * The measured state of the selected workspace's vector store. Every value
 * is a count the adapter reports; there is no rebuild action, so no refresh
 * button exists here. A store without a serving profile answers 503 and
 * renders as an unavailable state.
 */
export function VectorsPane({ workspace }: AdminPaneProps) {
  const [stats, setStats] = useState<VectorStats | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ApiError | null>(null);
  const [reloadKey, setReloadKey] = useState(0);

  const workspaceId = workspace?.workspaceId ?? null;
  useEffect(() => {
    const controller = new AbortController();
    let active = true;
    setStats(null);
    setLoading(true);
    setError(null);
    if (!workspaceId) {
      setLoading(false);
      return () => { active = false; controller.abort(); };
    }
    api.vectorStats(workspaceId, controller.signal)
      .then((result) => { if (active) setStats(result); })
      .catch((cause) => {
        if (active && !controller.signal.aborted) {
          setError(cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The vector stats could not be loaded."));
        }
      })
      .finally(() => { if (active) setLoading(false); });
    return () => { active = false; controller.abort(); };
  }, [workspaceId, reloadKey]);

  return (
    <>
      <header className="ui-admin__head">
        <div className="ui-admin__head-copy">
          <h1>Vector index</h1>
          <p>Measured state of the selected workspace. A real rebuild action does not exist, so no refresh control is shown.</p>
        </div>
      </header>

      {!workspaceId && (
        <div className="ui-admin-unavailable" role="status">
          <CircleAlert size={28} aria-hidden="true" />
          <strong>No workspace selected</strong>
          <span>Choose a workspace in the top bar to inspect its vector index.</span>
        </div>
      )}
      {workspaceId && error && (
        <div className="ui-admin-unavailable" role="status">
          <CircleAlert size={28} aria-hidden="true" />
          <strong>Vector index unavailable</strong>
          <span>{formatError(error)}</span>
          <Button size="sm" variant="ghost" onClick={() => setReloadKey((key) => key + 1)}>Retry</Button>
        </div>
      )}
      {workspaceId && loading && !error && <p className="ui-admin-state">Loading vector stats…</p>}
      {workspaceId && !loading && !error && stats && (
        <div className="ui-admin-stats">
          <div className="ui-admin-stat">
            <span className="ui-admin-stat__label">Embeddings</span>
            <span className="ui-admin-stat__value">{stats.embeddingCount}</span>
          </div>
          <div className="ui-admin-stat">
            <span className="ui-admin-stat__label">Dimensions</span>
            <span className="ui-admin-stat__value">{stats.dims}</span>
          </div>
          <div className="ui-admin-stat">
            <span className="ui-admin-stat__label">Graph nodes</span>
            <span className="ui-admin-stat__value">{stats.petgraphNodes}</span>
          </div>
          <div className="ui-admin-stat">
            <span className="ui-admin-stat__label">Graph edges</span>
            <span className="ui-admin-stat__value">{stats.petgraphEdges}</span>
          </div>
        </div>
      )}
    </>
  );
}
