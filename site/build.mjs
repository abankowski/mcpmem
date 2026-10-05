#!/usr/bin/env node
// Build the landing page: template.html + content/index.md -> dist/.
// Zero dependencies; CI runs this with a bare Node.
//
// The template holds the design's structure and CSS with {{slot:key}}
// placeholders. The content file holds one `key: value` per line. The
// build fails when a slot has no value or a value has no slot, so the
// template and the copy cannot drift silently.

import { readFileSync, writeFileSync, mkdirSync, copyFileSync, existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const dist = join(here, 'dist');
mkdirSync(dist, { recursive: true });

// 1. Parse content into a key -> value map.
const content = readFileSync(join(here, 'content', 'index.md'), 'utf8');
const values = new Map();
for (const rawLine of content.split('\n')) {
  const line = rawLine.trim();
  if (!line || line.startsWith('#') || line.startsWith('---')) continue;
  const sep = line.indexOf(':');
  if (sep < 1) throw new Error(`content line has no 'key: value': ${rawLine}`);
  const key = line.slice(0, sep).trim();
  const value = line.slice(sep + 1).trim();
  if (values.has(key)) throw new Error(`duplicate content key: ${key}`);
  values.set(key, value);
}

// 2. Fill the template.
let html = readFileSync(join(here, 'src', 'template.html'), 'utf8');
const used = new Set();
html = html.replace(/\{\{slot:([^}]+)\}\}/g, (marker, key) => {
  used.add(key);
  if (!values.has(key)) throw new Error(`template slot has no content value: ${key}`);
  return values.get(key);
});
for (const key of values.keys()) {
  if (!used.has(key)) throw new Error(`content key is not used by the template: ${key}`);
}

// 3. Assets and the custom domain.
for (const file of ['brain.svg', 'og.png']) {
  copyFileSync(join(here, 'src', file), join(dist, file));
}
if (existsSync(join(here, 'CNAME'))) {
  copyFileSync(join(here, 'CNAME'), join(dist, 'CNAME'));
}
// GitHub Pages never runs Jekyll on a deploy-pages artifact, but the empty
// marker also protects the branch-based fallback.
writeFileSync(join(here, 'dist', '.nojekyll'), '');
writeFileSync(join(here, 'dist', 'index.html'), html);
console.log(`built dist/ (${values.size} keys, ${used.size} slots)`);