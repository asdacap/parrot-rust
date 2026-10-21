#!/usr/bin/env node
// Collect only the already-installed locked frontend packages; never installs.
import { copyFileSync, existsSync, mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from "node:fs"
import { join } from "node:path"

const [modules, output] = process.argv.slice(2)
if (!modules || !output) throw new Error("usage: collect-web-notices.mjs NODE-MODULES OUTPUT")
const packages = new Map()
function visitPackage(root) {
  const manifest = join(root, "package.json")
  if (!existsSync(manifest)) return
  const pkg = JSON.parse(readFileSync(manifest, "utf8"))
  const identity = `${pkg.name}@${pkg.version}`
  if (!packages.has(identity)) {
    const destination = join(output, pkg.name.replaceAll("/", "+"), pkg.version)
    const files = readdirSync(root).filter(name =>
      /(^|[._-])(unlicen[cs]e|licen[cs]e|copying|notice|copyright)([._-]|$)/i.test(name) && statSync(join(root, name)).isFile())
    if (files.length) mkdirSync(destination, { recursive: true })
    for (const name of files) copyFileSync(join(root, name), join(destination, name))
    packages.set(identity, { name: pkg.name, version: pkg.version, license: pkg.license ?? "UNSPECIFIED", notice_files: files.sort() })
  }
  const nested = join(root, "node_modules")
  if (existsSync(nested)) visitModules(nested)
}
function visitModules(root) {
  for (const name of readdirSync(root).sort()) {
    if (name.startsWith(".")) continue
    const path = join(root, name)
    if (name.startsWith("@")) {
      for (const child of readdirSync(path).sort()) visitPackage(join(path, child))
    } else visitPackage(path)
  }
}
visitModules(modules)
mkdirSync(output, { recursive: true })
writeFileSync(join(output, "npm-notices.json"), JSON.stringify([...packages.values()].sort((a, b) =>
  `${a.name}@${a.version}`.localeCompare(`${b.name}@${b.version}`, "en")), null, 2) + "\n")
