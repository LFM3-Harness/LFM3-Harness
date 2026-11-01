// Stages the pinned CPU llama.cpp runtime for the Tauri installer.
//
// The installer must be usable immediately after setup. Only model weights are
// downloaded by the app; llama-server, llama-tts, their DLLs, the VC runtime
// they load, and the bundled licenses ship inside the installer.
//
// The full tools/llama folder also contains a ~600 MB CUDA backend that this
// CPU-profile app never uses, plus development tools. Copying the folder
// wholesale would make the installer enormous, so this script copies an
// explicit allowlist instead.
import { access, copyFile, mkdir, readdir, rm, stat, writeFile } from 'node:fs/promises';
import { constants } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const source = path.join(root, 'tools', 'llama');
const destination = path.join(
  root,
  'apps',
  'desktop',
  'src-tauri',
  'resources',
  'llama',
);

const coreFiles = [
  'llama-server.exe',
  'llama-tts.exe',
  'llama-server-impl.dll',
  'llama-common.dll',
  'llama.dll',
  'ggml.dll',
  'ggml-base.dll',
  'mtmd.dll',
  'libomp.dll',
  'LICENSE-LLVM-OpenMP',
];

// These are Microsoft Visual C++ redistributable files. llama.cpp is built with
// the MSVC ABI and imports them, but a clean Windows install may not have them
// on PATH. Shipping them beside llama-server makes the runtime self-contained;
// the app-local copy is found before the system copy.
const vcRuntimeFiles = ['vcruntime140.dll', 'vcruntime140_1.dll', 'msvcp140.dll'];

async function exists(file) {
  try {
    await access(file, constants.R_OK);
    return true;
  } catch {
    return false;
  }
}

async function copyRequired(name, from) {
  const file = path.join(from, name);
  if (!(await exists(file))) {
    throw new Error(
      `missing ${path.relative(root, file)}; stage the pinned llama.cpp b11960 runtime in tools/llama first`,
    );
  }
  await copyFile(file, path.join(destination, name));
  return (await stat(file)).size;
}

const cpuBackends = (await readdir(source).catch(() => []))
  .filter((name) => /^ggml-cpu-.*\.dll$/i.test(name))
  .sort();
if (cpuBackends.length === 0) {
  throw new Error(
    'no ggml-cpu-*.dll backends found in tools/llama; the installed app would not be able to run a model',
  );
}

await mkdir(destination, { recursive: true });
// Rebuild from scratch so a stale or accidentally added model can never ride
// along into the installer.
await rm(destination, { recursive: true, force: true });
await mkdir(destination, { recursive: true });

let copiedBytes = 0;
const copied = [];
for (const name of [...coreFiles, ...cpuBackends]) {
  copiedBytes += await copyRequired(name, source);
  copied.push(name);
}

const systemRoot = process.env.SystemRoot ?? process.env.WINDIR ?? 'C:\\Windows';
for (const name of vcRuntimeFiles) {
  let from = path.join(source, name);
  if (!(await exists(from))) {
    from = path.join(systemRoot, 'System32', name);
  }
  copiedBytes += await copyRequired(name, path.dirname(from));
  copied.push(name);
}

const runtimeNote = [
  'LFM3 Workspace bundled llama.cpp runtime',
  '',
  'llama.cpp b11960 / v0.8.0 (CPU profile, -ngl 0)',
  `Staged from: ${source}`,
  '',
  'Files:',
  ...copied.map((name) => `  ${name}`),
  '',
  'The CUDA backend, development tools, and all GGUF model weights are',
  'deliberately excluded. The app downloads only model weights, on request.',
  '',
].join('\n');
await writeFile(path.join(destination, 'RUNTIME.txt'), runtimeNote, 'utf8');

console.log(
  `staged ${copied.length} llama.cpp runtime files (${(copiedBytes / 1024 / 1024).toFixed(1)} MiB) in ${path.relative(root, destination)}`,
);
