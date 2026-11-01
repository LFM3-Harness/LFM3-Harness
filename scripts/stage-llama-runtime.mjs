// Stages the pinned CPU llama.cpp runtime for the Tauri installer.
//
// The installer must be usable immediately after setup. Only model weights are
// downloaded by the app; llama-server, llama-tts, their shared libraries, the
// VC runtime they load on Windows, and the bundled licenses ship inside the
// installer.
//
// The full tools/llama folder can contain a large CUDA backend this CPU-profile
// app never uses, plus development tools. Copying the folder wholesale would
// make the installer enormous, so this script copies an explicit allowlist
// instead. Windows and Linux archives have different shapes; both are handled
// here so the same build command works on either host.
import { access, chmod, copyFile, mkdir, readdir, rm, stat, writeFile } from 'node:fs/promises';
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

const platform = process.platform;
const windows = platform === 'win32';
const linux = platform === 'linux';
if (!windows && !linux) {
  throw new Error(
    `unsupported build host ${platform}; stage the llama.cpp runtime for Windows or Linux`,
  );
}

const windowsExecutables = ['llama-server.exe', 'llama-tts.exe'];
const windowsLibraries = [
  'llama-server-impl.dll',
  'llama-common.dll',
  'llama.dll',
  'ggml.dll',
  'ggml-base.dll',
  'mtmd.dll',
  'libomp.dll',
];
const windowsLicenses = ['LICENSE-LLVM-OpenMP'];
const linuxExecutables = ['llama-server', 'llama-tts'];

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

async function copyRequired(name, from, mode) {
  const file = path.join(from, name);
  if (!(await exists(file))) {
    throw new Error(
      `missing ${path.relative(root, file)}; stage the pinned llama.cpp b11960 runtime in tools/llama first`,
    );
  }
  const target = path.join(destination, name);
  await copyFile(file, target);
  if (!windows && mode !== undefined) {
    await chmod(target, mode);
  }
  return (await stat(file)).size;
}

async function copyIfPresent(name, from, mode) {
  if (!(await exists(path.join(from, name)))) {
    return 0;
  }
  const target = path.join(destination, name);
  await copyFile(path.join(from, name), target);
  if (!windows && mode !== undefined) {
    await chmod(target, mode);
  }
  return (await stat(path.join(from, name))).size;
}

if (!(await exists(source))) {
  throw new Error(
    windows
      ? 'tools/llama is missing; extract the pinned llama.cpp b11960 Windows archive there first'
      : 'tools/llama is missing; download llama-b11960-bin-ubuntu-x64.tar.gz from the llama.cpp b11960 release and extract it into tools/llama',
  );
}

const entries = await readdir(source, { withFileTypes: true });
const sourceNames = new Set(entries.map((entry) => entry.name));

await rm(destination, { recursive: true, force: true });
await mkdir(destination, { recursive: true });

let copiedBytes = 0;
const copied = [];

if (windows) {
  const cpuBackends = [...sourceNames]
    .filter((name) => /^ggml-cpu-.*\.dll$/i.test(name))
    .sort();
  if (cpuBackends.length === 0) {
    throw new Error(
      'no ggml-cpu-*.dll backends found in tools/llama; the installed app would not be able to run a model',
    );
  }

  for (const name of [...windowsExecutables, ...windowsLibraries, ...cpuBackends]) {
    copiedBytes += await copyRequired(name, source);
    copied.push(name);
  }

  const systemRoot = process.env.SystemRoot ?? process.env.WINDIR ?? 'C:\\Windows';
  for (const name of vcRuntimeFiles) {
    let from = source;
    if (!(await exists(path.join(source, name)))) {
      from = path.join(systemRoot, 'System32');
    }
    copiedBytes += await copyRequired(name, from);
    copied.push(name);
  }
  for (const name of windowsLicenses) {
    copiedBytes += await copyIfPresent(name, source);
    if (await exists(path.join(source, name))) {
      copied.push(name);
    }
  }
} else {
  for (const name of linuxExecutables) {
    copiedBytes += await copyRequired(name, source, 0o755);
    copied.push(name);
  }

  // Keep the shared runtime and CPU backends. GPU backends in the upstream
  // archive are not used by this CPU-profile app and would only bloat it.
  const gpuBackend = /(cuda|vulkan|hip|metal|kompute|sycl|opencl|rpc)/i;
  const libraries = [...sourceNames]
    .filter((name) => /^lib.*\.so(\.\d+)*$/i.test(name) && !gpuBackend.test(name))
    .sort();
  if (libraries.length === 0) {
    throw new Error(
      'no lib*.so* shared libraries found in tools/llama; the installed app would not be able to run a model',
    );
  }
  for (const name of libraries) {
    copiedBytes += await copyRequired(name, source, 0o644);
    copied.push(name);
  }

  for (const name of ['LICENSE', 'LICENSE-LLVM-OpenMP']) {
    copiedBytes += await copyIfPresent(name, source, 0o644);
    if (await exists(path.join(source, name))) {
      copied.push(name);
    }
  }
}

const runtimeNote = [
  'LFM3 Workspace bundled llama.cpp runtime',
  '',
  `llama.cpp b11960 / v0.8.0 (CPU profile, -ngl 0), ${platform}`,
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
  `staged ${copied.length} llama.cpp runtime files (${(copiedBytes / 1024 / 1024).toFixed(1)} MiB) for ${platform} in ${path.relative(root, destination)}`,
);
