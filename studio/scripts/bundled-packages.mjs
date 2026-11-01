// Lists the npm packages a Studio build actually bundles: runs the build without writing it and
// prints one JSON line per package (directory, name, version, license), worker bundles included.
// Used to keep THIRD-PARTY-NOTICES to what ships.
//
//   node scripts/bundled-packages.mjs [mode]    (mode: server, the default, or wasm)

import { readFileSync } from 'node:fs'
import { dirname, join, sep } from 'node:path'
import { fileURLToPath } from 'node:url'
import { build } from 'vite'

const studio = join(dirname(fileURLToPath(import.meta.url)), '..')
const mode = process.argv[2] ?? 'server'
const dirs = new Set()

/** The package directory holding a module, by its last `node_modules` segment. */
function packageDir(id) {
  const path = id.replace(/\?.*$/, '').replaceAll('/', sep)
  const marker = `${sep}node_modules${sep}`
  const at = path.lastIndexOf(marker)
  if (at < 0) return null
  const rest = path.slice(at + marker.length).split(sep)
  const parts = rest[0].startsWith('@') ? rest.slice(0, 2) : rest.slice(0, 1)
  return path.slice(0, at + marker.length) + parts.join(sep)
}

const collect = {
  name: 'collect-bundled-packages',
  generateBundle(_, bundle) {
    for (const output of Object.values(bundle)) {
      for (const id of output.type === 'chunk' ? output.moduleIds : []) {
        const dir = packageDir(id)
        if (dir) dirs.add(dir)
      }
    }
  },
}

await build({
  root: studio,
  mode,
  logLevel: 'silent',
  build: { write: false },
  plugins: [collect],
  worker: { plugins: () => [collect] },
})

for (const dir of [...dirs].sort()) {
  const pkg = JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8'))
  const license = typeof pkg.license === 'object' ? pkg.license?.type : pkg.license
  console.log(JSON.stringify({ dir, name: pkg.name, version: pkg.version, license: license ?? null }))
}
