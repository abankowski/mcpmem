# Landing page

The public marketing page for mcpmem, built from the design handoff in
`designs/mcpmem-ui-handoff/landing/`. A zero-dependency Node build renders
the design template with markdown content into a static `dist/`, and a
GitHub Actions workflow publishes it to GitHub Pages.

## Layout

| Path | What it is |
|---|---|
| `src/template.html` | the design's HTML, byte-for-byte, with `{{slot:key}}` placeholders |
| `content/index.md` | the page copy: one `key: value` line per slot |
| `build.mjs` | the build; fails on a missing or unused key, so template and copy cannot drift |
| `src/brain.svg` | the hero illustration (from the design) |
| `src/og.png` | 1200x630 social card (a crop of the landing render) |
| `CNAME` | optional; one line with the custom domain, copied into `dist/` when present |
| `dist/` | build output; generated, never committed |

## Editing the copy

Edit `content/index.md`, run the build, and open `dist/index.html`:

```sh
cd site && node build.mjs
```

Every slot has a default from the design, so the build always produces a
complete page. Add a key without using it, or a slot without a value, and
the build stops and names the offender.

## Publishing

The `pages` workflow (`.github/workflows/pages.yml`) builds and publishes
on every push to `main` that touches `site/**` or the workflow itself, and
on manual dispatch. GitHub Pages must be enabled for the repository
(Settings → Pages → "GitHub Actions" as the source) once.

### Custom domain

1. Create `site/CNAME` with the domain on one line, e.g. `mcpmem.example.com`.
2. Add a `CNAME` DNS record from that domain to `<owner>.github.io`.
3. In Settings → Pages, set the custom domain (this writes the DNS' TXT
   verification and enables HTTPS).

Without a `CNAME` the site publishes at `<owner>.github.io/mcpmem/`.