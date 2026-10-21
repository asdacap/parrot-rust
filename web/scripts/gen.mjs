import { execFileSync } from "node:child_process"
import { mkdirSync, rmSync } from "node:fs"
import process from "node:process"

const outDir = "src/gen"
const plugin = process.platform === "win32" ? "node_modules/.bin/protoc-gen-es.cmd" : "node_modules/.bin/protoc-gen-es"

rmSync(outDir, { recursive: true, force: true })
mkdirSync(outDir, { recursive: true })
execFileSync(
  process.env.PROTOBUF_PROTOC ?? "protoc",
  [
    `--plugin=protoc-gen-es=${plugin}`,
    `--es_out=${outDir}`,
    "--es_opt=target=ts",
    "-I", "../proto",
    "parrot.proto",
    "web.proto",
    "queue.proto",
    "process.proto",
  ],
  { stdio: "inherit" },
)
