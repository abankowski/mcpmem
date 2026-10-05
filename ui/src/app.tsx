import { useCallback, useEffect, useState, type ComponentType, type FormEvent, type ReactNode } from "react";
import { Brain, CircleAlert, RotateCw } from "lucide-react";
import { api, ApiError } from "./lib/api";
import { beginAuth, canAuthorize, captureHashToken, completeAuthCallback, requestConsent, setGraphScopes, setStaticToken } from "./lib/auth";
import { shellContext } from "./lib/app-context";
import { currentWorkspace, invalidateWorkspaces, loadWorkspaces, selectWorkspace } from "./lib/workspaces";
import { PAGE_PATHS, pageUrl, type Page } from "./lib/urls";
import type { Session, Workspace } from "./lib/schemas";
import { Button } from "./components/Button";
import { CommandPalette } from "./components/CommandPalette";
import { ToastProvider } from "./components/Toast";
import { TopBar } from "./components/TopBar";
import "./components/shell.css";

type PageModule = { Page: ComponentType };
const pages = import.meta.glob<PageModule>("./features/*/page.tsx", { eager: false });
// React StrictMode runs the mount effect twice. Reuse one token exchange;
// an OAuth authorization code must never be exchanged twice.
let initialAuth: Promise<string | null> | null = null;

function finishInitialAuth(): Promise<string | null> {
  if (!initialAuth) {
    captureHashToken();
    initialAuth = completeAuthCallback();
  }
  return initialAuth;
}

function routePage(pathname: string): Page | null {
  if (pathname === PAGE_PATHS.graph) return "graph";
  if (pathname === PAGE_PATHS.search) return "search";
  if (pathname === PAGE_PATHS.admin || pathname === PAGE_PATHS.adminCallback) return "admin";
  return null;
}

function EmptyState({ title, message, children, error }: { title: string; message: string; children?: ReactNode; error?: boolean }) {
  return (
    <main className="ui-page ui-page--empty" id="content">
      <section className="ui-empty" aria-label={title}>
        {error ? <CircleAlert size={36} aria-hidden="true" /> : <Brain size={36} aria-hidden="true" />}
        <h1>{title}</h1>
        <p className={error ? "ui-empty__error" : undefined}>{message}</p>
        {children && <div className="ui-empty__actions">{children}</div>}
      </section>
    </main>
  );
}

function Shell() {
  const [page, setPage] = useState<Page | null>(() => routePage(location.pathname));
  const [workspace, setWorkspace] = useState<Workspace | null>(currentWorkspace);
  const [workspaces, setWorkspaces] = useState<readonly Workspace[]>([]);
  const [session, setSession] = useState<Session | null>(null);
  const [adminSession, setAdminSession] = useState<Session | null>(null);
  const [graphError, setGraphError] = useState<ApiError | null>(null);
  const [adminError, setAdminError] = useState<ApiError | null>(null);
  const [callbackError, setCallbackError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadedPage, setLoadedPage] = useState<PageModule | null>(null);
  const [pageError, setPageError] = useState(false);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [tokenInput, setTokenInput] = useState("");
  const [revision, setRevision] = useState(0);
  const reload = useCallback(() => setRevision((previous) => previous + 1), []);

  useEffect(() => {
    const controller = new AbortController();
    let active = true;
    async function bootstrap() {
      setLoading(true);
      setGraphError(null);
      setAdminError(null);
      setSession(null);
      setAdminSession(null);
      try {
        const callback = await finishInitialAuth();
        if (!active) return;
        setCallbackError(callback);
        const resolvedPage = routePage(location.pathname);
        setPage(resolvedPage);
        try {
          const items = await loadWorkspaces(false, controller.signal);
          if (!active) return;
          setWorkspaces(items);
          const selected = currentWorkspace();
          setWorkspace(selected);
          const graphSession = await api.session(selected?.workspaceId, "graph", controller.signal);
          if (!active) return;
          setSession(graphSession);
          setGraphScopes(graphSession.scopes);
        } catch (cause) {
          if (active && !controller.signal.aborted) {
            setGraphError(cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The workspace or session request failed."));
            setGraphScopes([]);
            if (cause instanceof ApiError && cause.status === 404) {
              invalidateWorkspaces();
              try {
                const fresh = await loadWorkspaces(true, controller.signal);
                if (active) {
                  setWorkspaces(fresh);
                  setWorkspace(currentWorkspace());
                }
              } catch {
                if (active) { setWorkspaces([]); setWorkspace(null); }
              }
            } else if (cause instanceof ApiError && cause.status === 401) {
              invalidateWorkspaces();
              setWorkspaces([]);
              setWorkspace(null);
            }
          }
        }
        if (resolvedPage === "admin") {
          try {
            const currentAdminSession = await api.session(undefined, "admin", controller.signal);
            if (active) setAdminSession(currentAdminSession);
          } catch (cause) {
            if (active && !controller.signal.aborted) setAdminError(cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The admin session request failed."));
          }
        }
      } catch (cause) {
        if (active && !controller.signal.aborted) {
          setCallbackError(cause instanceof Error ? cause.message : "Sign-in could not be completed.");
        }
      } finally {
        if (active) setLoading(false);
      }
    }
    void bootstrap();
    return () => { active = false; controller.abort(); };
  }, [revision]);

  useEffect(() => {
    const onPopState = () => { setPage(routePage(location.pathname)); reload(); };
    window.addEventListener("popstate", onPopState);
    return () => window.removeEventListener("popstate", onPopState);
  }, [reload]);

  useEffect(() => {
    if (!page) { setLoadedPage(null); return; }
    const key = `./features/${page}/page.tsx`;
    const load = pages[key];
    if (!load) { setLoadedPage(null); setPageError(false); return; }
    let active = true;
    setLoadedPage(null);
    setPageError(false);
    load().then((module) => {
      if (!active) return;
      if (!module.Page) {
        // Feature modules such as the Files panel export named components,
        // not Page. Skip them and keep the route's fallback state.
        console.warn(`Skipping page module ${key}: it does not export Page.`);
        setLoadedPage(null);
        setPageError(false);
        return;
      }
      setLoadedPage(module);
    })
      .catch(() => { if (active) setPageError(true); });
    return () => { active = false; };
  }, [page]);

  useEffect(() => {
    const openCommand = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setPaletteOpen((open) => !open);
      }
    };
    window.addEventListener("keydown", openCommand);
    return () => window.removeEventListener("keydown", openCommand);
  }, []);

  const switchWorkspace = useCallback((id: string) => {
    selectWorkspace(id);
    setWorkspace(currentWorkspace());
    reload();
  }, [reload]);

  function signInWithToken(event: FormEvent<HTMLFormElement>): void {
    event.preventDefault();
    if (!tokenInput.trim()) return;
    setStaticToken(tokenInput);
    setTokenInput("");
    invalidateWorkspaces();
    reload();
  }

  function pickNode(name: string): void {
    const target = pageUrl("graph");
    target.searchParams.set("node", name);
    if (workspace) target.searchParams.set("workspaceId", workspace.workspaceId);
    location.assign(target.href);
  }

  let content: ReactNode;
  if (loading) {
    content = <EmptyState title="Loading workspace" message="Reading your session and accessible workspaces." />;
  } else if (!page) {
    content = <EmptyState title="Page not found" message="This page is not part of the browser UI." error />;
  } else if (callbackError) {
    content = <EmptyState title="Sign-in failed" message={callbackError} error>
      <Button onClick={() => { void beginAuth(page === "admin" ? "admin" : "graph"); }}>Sign in again</Button>
    </EmptyState>;
  } else if (page !== "admin" && graphError?.status === 401) {
    content = (
      <EmptyState title="Authentication required" message="Sign in for graph access or enter a configured bearer token." error>
        {canAuthorize("graph") && <Button variant="primary" onClick={() => { void beginAuth("graph"); }}>Sign in</Button>}
        <form onSubmit={signInWithToken} className="ui-empty__actions">
          <label htmlFor="static-token">Bearer token</label>
          <input id="static-token" type="password" value={tokenInput} onChange={(event) => setTokenInput(event.target.value)} autoComplete="off" />
          <Button type="submit">Use token</Button>
        </form>
      </EmptyState>
    );
  } else if (page !== "admin" && graphError?.status === 403 && graphError.code === "insufficient_scope") {
    content = <EmptyState title="More access needed" message="Graph reading needs graph-read consent." error>
      {canAuthorize("graph") && <Button onClick={() => { void requestConsent(["graph-read"]); }}>Request graph-read</Button>}
    </EmptyState>;
  } else if (page !== "admin" && graphError?.status === 403) {
    content = <EmptyState title="Permission denied" message={graphError.message} error />;
  } else if (page !== "admin" && graphError?.status === 503) {
    content = <EmptyState title="Service unavailable" message={graphError.message} error><Button onClick={reload}>Retry</Button></EmptyState>;
  } else if (page !== "admin" && graphError) {
    content = <EmptyState title="Workspace unavailable" message={graphError.message} error><Button onClick={reload}><RotateCw size={16} aria-hidden="true" />Retry</Button></EmptyState>;
  } else if (page !== "admin" && !workspace) {
    content = <EmptyState title={workspaces.length ? "Select a workspace" : "No workspaces yet"}
      message={workspaces.length ? "Choose an accessible workspace in the top bar." : "Create a workspace or ask its owner for access."} />;
  } else if (page === "admin" && adminError?.status === 401) {
    content = <EmptyState title="Admin sign-in needed" message="Admin access requires a separate human OAuth session." error>
      {canAuthorize("admin") && <Button variant="primary" onClick={() => { void beginAuth("admin"); }}>Sign in as admin</Button>}
    </EmptyState>;
  } else if (page === "admin" && !adminSession && adminError) {
    content = <EmptyState title="Admin unavailable" message={adminError.message} error><Button onClick={reload}>Retry</Button></EmptyState>;
  } else if (page === "admin" && !adminSession) {
    content = <EmptyState title="Admin access" message="Sign in with an account that holds the admin scope." />;
  } else if (pageError) {
    content = <EmptyState title="Page unavailable" message="The page module could not load." error><Button onClick={() => location.reload()}>Retry</Button></EmptyState>;
  } else if (!loadedPage) {
    content = <EmptyState title={page === "graph" ? "Graph" : page === "search" ? "Search" : "Admin"}
      message="No page view is installed for this route." />;
  } else {
    const Screen = loadedPage.Page;
    content = <main key={`${page}:${workspace?.workspaceId ?? "none"}`} className="ui-page" id="content"><Screen /></main>;
  }

  return (
    <shellContext.Provider value={{ workspace, session, adminSession, reload, selectWorkspace: switchWorkspace }}>
      <div className="ui-shell">
        <a className="ui-visually-hidden ui-skip-link" href="#content">Skip to content</a>
        <TopBar page={page ?? "graph"} current={workspace} workspaces={workspaces}
          principalName={session?.principalName ?? adminSession?.principalName ?? null}
          onWorkspaceChange={switchWorkspace} onOpenCommand={() => setPaletteOpen(true)} />
        {content}
        <CommandPalette open={paletteOpen} workspaceId={workspace?.workspaceId ?? null}
          onClose={() => setPaletteOpen(false)} onPick={pickNode} />
      </div>
    </shellContext.Provider>
  );
}

export function App() {
  return <ToastProvider><Shell /></ToastProvider>;
}
