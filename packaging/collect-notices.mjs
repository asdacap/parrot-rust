#!/usr/bin/env node
// Build-time notices only: no network requests or package resolution.
import { copyFileSync, existsSync, mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from "node:fs"
import { basename, dirname, join, resolve } from "node:path"

const [metadataPath, output] = process.argv.slice(2)
if (!metadataPath || !output) throw new Error("usage: collect-notices.mjs CARGO-METADATA OUTPUT")
const metadata = JSON.parse(readFileSync(metadataPath, "utf8"))
mkdirSync(output, { recursive: true })
const notices = metadata.packages
  .slice()
  .sort((a, b) => `${a.name}@${a.version}`.localeCompare(`${b.name}@${b.version}`, "en"))
  .map(pkg => {
    const root = dirname(pkg.manifest_path)
    const candidates = readdirSync(root)
      .filter(name => /(^|[._-])(unlicen[cs]e|licen[cs]e|copying|notice|copyright)([._-]|$)/i.test(name))
      .map(name => join(root, name))
    if (pkg.license_file) candidates.push(resolve(root, pkg.license_file))
    const destination = join(output, `${pkg.name}-${pkg.version}`)
    const files = []
    for (const source of [...new Set(candidates)].sort()) {
      if (!existsSync(source) || !statSync(source).isFile()) continue
      mkdirSync(destination, { recursive: true })
      const name = basename(source)
      copyFileSync(source, join(destination, name))
      files.push(name)
    }
    return {
      name: pkg.name,
      version: pkg.version,
      source: pkg.source,
      license: pkg.license ?? "UNSPECIFIED",
      notice_files: [...new Set(files)],
    }
  })
writeFileSync(join(output, "cargo-notices.json"), JSON.stringify(notices, null, 2) + "\n")
writeFileSync(join(output, "README.txt"),
  "Cargo package license expressions and available upstream notice files.\n" +
  "UNSPECIFIED is not a license grant; review unresolved project/dependency terms before redistribution.\n" +
  "Build/target-specific dependencies may be included; this inventory is not a linked-library list.\n")
