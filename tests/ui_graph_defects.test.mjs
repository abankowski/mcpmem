#!/usr/bin/env node
// Targeted regression tests for the four graph.js attachment defects fixed on
// the repair/ui branch. This repo has no JS test runner (no package.json, no
// lint), so these are source-level assertions: each test pairs the defect to
// the code shape that implements its consumer-visible contract and fails if a
// future change reintroduces the old shape.
//
// Run (from the worktree root):  node tests/ui_graph_defects.test.mjs
// Exit 0 = all checks pass; exit 1 = a defect is present (or syntax is broken).
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const path = fileURLToPath(new URL("../src/ui/graph.js", import.meta.url));
const src = readFileSync(path, "utf8");

let failures = 0;
function check(name, ok, detail) {
  console.log(`[${ok ? "PASS" : "FAIL"}] ${name}`);
  if (!ok) { console.log(`       ${detail}`); failures++; }
}

// Syntax sanity: parsing without executing (document/canvas are not available).
try { new Function(src); check("graph.js parses", true, ""); }
catch (e) { check("graph.js parses", false, String(e)); }

// Defect 1 — the Next Page button must follow the attachment's page metadata,
// not /pages eof (eof means "current page has no more text": a two-page PDF
// whose first page fits one response locks Next at once; a long single page
// arms Next before the rest of the page is read).
{
  const m = /"attNextPage"\)\.disabled\s*=\s*(?!\s*true\b)([^;]+);/.exec(src);
  check("Next Page compares viewer.page with pageCount (not data.eof)",
    m && m[1].includes("pageCount") && m[1].includes("viewer.page") && !m[1].includes("data.eof"),
    m ? `disabled = ${m[1].trim()}` : "no attNextPage disabled assignment found");
  check("viewer stores pageCount from the row metadata",
    /attachmentViewer\s*=\s*\{[^}]*pageCount:\s*row\.pageCount/.test(src),
    "openAttachmentViewer does not store row.pageCount");
}

// Defect 2 — the attachments list request must send a limit; the server's
// default is 100 (clamped 1..=1000), which silently drops the oldest files
// and their Read/Download/Delete controls from the inspector.
{
  const m = /entityName:\s*ctx\.node\.id\s*,\s*limit:\s*"(\d+)"/.exec(src);
  check("list request sends limit >= 1000", m && Number(m[1]) >= 1000,
    m ? `limit=${m[1]}` : "no limit parameter on the /ui/attachments list request");
}

// Defect 3 — the OAuth return must restore the saved node even when it is not
// on the reloaded browse page (a node the user added by expansion): reload it
// by name instead of dropping the pending inspector.
{
  check("resume path restores the node by name",
    /const node = nodeById\.get\(resume\.entityName\);[\s\S]{0,500}?}\s*else\s*\{[\s\S]{0,500}?restoreInspectorNode\(resume\.entityName,\s*workspace\.generation\)/.test(src),
    "pendingInspector resume block has no fallback for a node missing from the browse page");
  check("restoreInspectorNode expands by name then reopens the inspector",
    /function restoreInspectorNode\(name, generation\) \{[\s\S]*?\/ui\/expand[\s\S]*?requestAttachmentConsent\(false\);/.test(src),
    "restoreInspectorNode does not expand by name and re-request attachment consent");
}

// Defect 4 — the upload response carries attachmentId + status only; the
// confirmation must name the selected file, not a response field.
{
  check("upload confirmation uses the selected file name",
    src.includes('"Upload started for " + file.name'),
    'confirmation must read "Upload started for " + file.name');
  check("no reference to uploaded.filename", !src.includes("uploaded.filename"),
    "uploaded.filename must be gone (the response has no filename field)");
}

console.log(failures ? `\n${failures} check(s) failed.` : "\nAll checks passed.");
process.exit(failures ? 1 : 0);