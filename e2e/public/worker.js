/**
 * wasm-git OPFS Web Worker.
 *
 * Message API:
 *   clone              { url }                 → { dircontents }
 *   writecommitandpush { filename, contents }  → { dircontents }
 *   readfile           { filename }            → { filename, filecontents }
 *   deletelocal        {}                      → { deleted }
 */

let stdout = [];
let stderr = [];

globalThis.wasmGitModuleOverrides = {
    print:    (text) => { console.log(text);   stdout.push(text); },
    printErr: (text) => { console.error(text); stderr.push(text); },
};

const lg2mod = await import(new URL('lg2_opfs.js', import.meta.url));
const lg = await lg2mod.default();
const FS = lg.FS;

// Set up git config
try { FS.mkdir('/home'); } catch (e) {}
try { FS.mkdir('/home/web_user'); } catch (e) {}
FS.writeFile('/home/web_user/.gitconfig',
    `[user]\nname = Test User\nemail = test@example.com`);

// Create OPFS backend and working directory
const backend = lg._lg2_create_opfs_backend();
if (!backend) throw new Error('Failed to create OPFS backend');

const workingDir = '/opfs';
const mkdirResult = lg.ccall(
    'lg2_create_directory', 'number',
    ['string', 'number', 'number'],
    [workingDir, 0o777, backend]
);
if (mkdirResult !== 0) throw new Error('Failed to create OPFS directory: ' + mkdirResult);
FS.chdir(workingDir);

let currentRepoDir;

function rmdirRecursive(p) {
    for (const entry of FS.readdir(p).filter(e => e !== '.' && e !== '..')) {
        const full = p + '/' + entry;
        try { FS.readdir(full); rmdirRecursive(full); } catch (e) { FS.unlink(full); }
    }
    FS.rmdir(p);
}

function createMountPointSymlink(repoName) {
    try { FS.unlink('/' + repoName); } catch (e) {}
    FS.symlink(workingDir + '/' + repoName, '/' + repoName);
}

onmessage = async (msg) => {
    stdout = []; stderr = [];
    const { command } = msg.data;

    try {
        if (command === 'clone') {
            const repoName = msg.data.url.substring(msg.data.url.lastIndexOf('/') + 1);
            currentRepoDir = workingDir + '/' + repoName;
            try { rmdirRecursive(currentRepoDir); } catch (e) {}
            try {
                const opfsRoot = await navigator.storage.getDirectory();
                await opfsRoot.removeEntry(repoName, { recursive: true });
            } catch (e) {}
            const cloneRet = lg.callMain(['clone', msg.data.url, currentRepoDir]);
            if (cloneRet !== 0) {
                postMessage({ error: `clone exited with code ${cloneRet}`, stdout, stderr });
                return;
            }
            createMountPointSymlink(repoName);
            FS.chdir(currentRepoDir);
            postMessage({ dircontents: FS.readdir('.'), stdout, stderr });

        } else if (command === 'writecommitandpush') {
            FS.chdir(currentRepoDir);
            FS.writeFile(msg.data.filename, msg.data.contents);
            FS.chdir(currentRepoDir);
            const addRet = lg.callMain(['add', '--verbose', msg.data.filename]);
            if (addRet !== 0) {
                postMessage({ error: `git add exited with code ${addRet}`, stdout, stderr });
                return;
            }
            FS.chdir(currentRepoDir);
            const commitRet = lg.callMain(['commit', '-m', `add ${msg.data.filename}`]);
            if (commitRet !== 0) {
                postMessage({ error: `git commit exited with code ${commitRet}`, stdout, stderr });
                return;
            }
            FS.chdir(currentRepoDir);
            const pushRet = lg.callMain(['push']);
            if (pushRet !== 0) {
                postMessage({ error: `git push exited with code ${pushRet}`, stdout, stderr });
                return;
            }
            FS.chdir(currentRepoDir);
            postMessage({ dircontents: FS.readdir('.'), stdout, stderr });

        } else if (command === 'readfile') {
            FS.chdir(currentRepoDir);
            postMessage({
                filename: msg.data.filename,
                filecontents: FS.readFile(msg.data.filename, { encoding: 'utf8' }),
            });

        } else if (command === 'deletelocal') {
            const repoName = currentRepoDir ? currentRepoDir.split('/').pop() : null;
            try { FS.chdir(workingDir); if (currentRepoDir) rmdirRecursive(currentRepoDir); } catch (e) {}
            if (repoName) {
                try {
                    const opfsRoot = await navigator.storage.getDirectory();
                    await opfsRoot.removeEntry(repoName, { recursive: true });
                } catch (e) {}
                try { FS.unlink('/' + repoName); } catch (e) {}
            }
            currentRepoDir = undefined;
            postMessage({ deleted: repoName });
        }
    } catch (e) {
        postMessage({ error: e.message, stdout, stderr });
    }
};

postMessage({ ready: true });
