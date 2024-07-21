/**
 * This script runs before `Tauri build` step.
 */

import fs from "node:fs";
import * as async from "async";
import * as Bun from "bun";
import decompress from "decompress";
import { minimatch } from "minimatch";
import { findRepositoryRoot } from "./common";
import type { GitHubAsset, GitHubRelease } from "./types/downloadMemosBuildsHook";

/**
 * Convert a GOOS-GOARCH build file name to a Rust target triple.
 *
 * Sample target triples:
 *
 * - `x86_64-pc-windows-msvc`
 * - `x86_64-unknown-linux-gnu`
 * - `x86_64-apple-darwin`
 * - `aarch64-apple-darwin`
 * @param file The file name.
 * @returns The target triple.
 */
export function makeTripletFromFileName(filename: string): string {
    const osList = ["darwin", "linux", "windows"];
    const platformMap: Record<string, string> = {
        windows: "pc",
        linux: "unknown",
        darwin: "apple"
    };
    const archMap: Record<string, string> = {
        x86_64: "x86_64",
        x64: "x86_64",
        x86: "i686",
        "386": "i686",
        arm64: "aarch64",
        aarch64: "aarch64",
        riscv64: "riscv64gc"
    };
    const variantMap: Record<string, string> = {
        windows: "msvc",
        linux: "gnu"
    };

    const os = osList.find((os) => filename.includes(os)) || "unknown";
    const platform =
        Object.entries(platformMap)
            .find(([key]) => filename.includes(key))
            ?.pop() || "unknown";
    const arch =
        Object.entries(archMap)
            .find(([key]) => filename.includes(key))
            ?.pop() || "unknown";
    const variant =
        Object.entries(variantMap)
            .find(([key]) => filename.includes(key))
            ?.pop() || "";

    const triplet = [arch, platform, os, variant].join("-");
    return triplet.endsWith("-") ? triplet.slice(0, -1) : triplet;
}

/**
 * Calculate the sha256 hex digest of a given file.
 *
 * Note: Uses Bun-specific APIs.
 * @param filePath The path to the file.
 * @returns The sha256 hex digest of the file.
 */
export async function sha256File(filePath: string): Promise<string> {
    const file = Bun.file(filePath);
    const hasher = new Bun.CryptoHasher("sha256");
    const buffer = await file.arrayBuffer();
    hasher.update(buffer);
    return hasher.digest("hex");
}

/**
 * Parse a sum file.
 *
 * Note: Uses Bun-specific APIs.
 * @param source - The path to the sum file. Might be a URL or local file.
 * @returns Map of files and their sums if valid JSON; otherwise empty object (`{}`).
 */
export async function parseSha256Sums(source: string): Promise<Record<string, string>> {
    let sha256Sums: string;
    // check if source is a URL or file path and handle accordingly (e.g., download the content)
    if (!source.startsWith("http") && fs.existsSync(source)) {
        if (process.versions.bun) {
            const file = Bun.file(source);
            sha256Sums = await file.text();
        } else {
            sha256Sums = fs.readFileSync(source, "utf8");
        }
    } else {
        const response = await fetch(source, { redirect: "follow", method: "GET" });
        sha256Sums = await response.text();
    }

    const lines = sha256Sums.split("\n");
    const fileHashes: Record<string, string> = {};
    for (const line of lines) {
        if (line.length === 0) {
            continue;
        }
        const elements = line.split("  ");
        const hash = elements[0].trim();
        const fileName = elements[1].trim();
        fileHashes[fileName] = hash;
    }
    return fileHashes;
}

/**
 * Download a file from a URL and save it to a local file.
 * @param srcURL The URL of the file to download.
 * @param dstFile The local file path to save the downloaded file.
 */
export async function downloadFile(srcURL: string, dstFile: string) {
    const file = Bun.file(dstFile);
    const writer = file.writer();

    await fetch(srcURL, { redirect: "follow" }).then(async (response) => {
        if (!response.ok) {
            throw new Error(`Failed to download file: ${response.statusText}`);
        }
        const reader = response.body?.getReader();
        while (reader) {
            const { done, value } = await reader.read();
            if (done) {
                break;
            }
            writer.write(value);
        }
    });
    writer.end();
}

async function downloadServerBinaries() {
    const repo = "memospot/memos-builds";
    const repoUrl = `https://api.github.com/repos/${repo}/releases/latest`;

    // Match only platforms that Memospot also supports
    const downloadFilesGlob = [
        "memos-*-darwin-arm64.tar.gz",
        "memos-*-darwin-x86_64.tar.gz",
        "memos-*-linux-x86_64.tar.gz",
        "memos-*-windows-x86_64.zip"
    ];

    // Fetch data from GitHub API.
    const response = await fetch(repoUrl, { method: "GET", redirect: "follow" });
    if (!response.ok) {
        throw new Error(`Failed to fetch GitHub release: ${response.statusText}`);
    }
    const ghRelease = (await response.json()) as GitHubRelease;
    const releaseAssets = ghRelease.assets as GitHubAsset[];

    if (!releaseAssets || releaseAssets.length === 0) {
        throw new Error("Failed to fetch assets");
    }

    console.log(`\x1b[34mMatching GitHub assets from ${repo}:${ghRelease.tag_name}...\x1b[0m`);

    const sha256sums = releaseAssets.find((asset) => {
        return asset.name.endsWith("SHA256SUMS.txt");
    })?.browser_download_url;
    if (!sha256sums) {
        throw new Error("Failed to find SHA256SUMS.txt");
    }

    const selectedFiles = releaseAssets.filter((asset) => {
        return downloadFilesGlob.some((mask) => minimatch(asset.name, mask));
    });

    if (selectedFiles.length === 0) {
        throw new Error("Failed to match files");
    }

    console.log(`\x1b[32mMatched ${selectedFiles.length} files\x1b[0m`);

    // Download files in parallel.
    console.log("\x1b[34mDownloading GitHub assets...\x1b[0m");
    await async
        .eachLimit(selectedFiles, 5, async (ghAsset: GitHubAsset) => {
            const fileName = ghAsset.name;
            const dstPath = `./server-dist/${fileName}`;

            if (fs.existsSync(dstPath)) {
                fs.rmSync(dstPath, { force: true, recursive: true });
            }
            process.stdout.write(`${ghAsset.browser_download_url}\n`);
            await downloadFile(ghAsset.browser_download_url, dstPath)
                .then(() => {
                    console.log(`\x1b[32m[OK]\x1b[0m Downloaded \x1b[36m${fileName}\x1b[0m`);
                })
                .catch((error) => {
                    console.log(`\x1b[31m[ERROR] ${fileName}\x1b[0m: ${error}`);
                    throw error;
                });
        })
        .catch((error: any) => {
            throw error;
        });

    // Check hashes via memos_SHA256SUMS.txt.
    const fileHashes = await parseSha256Sums(sha256sums);
    console.log("\x1b[34mChecking downloaded file hashes...\x1b[0m");
    await async
        .eachLimit(selectedFiles, 2, async (file: GitHubAsset) => {
            const fileName = file.name;

            const filePath = `./server-dist/${fileName}`;
            const fileHash = await sha256File(filePath);

            if (fileHash !== fileHashes[fileName]) {
                console.log(`\x1b[31m[ERROR]\x1b[0m ${fileName} \x1b[36m${fileHash}\x1b[0m`);
                throw new Error(
                    `Hash mismatch for ${fileName}. Expected: ${fileHashes[fileName]}, got: ${fileHash}`
                );
            }
            console.log(`\x1b[32m[OK]\x1b[0m ${fileName} \x1b[36m${fileHash}\x1b[0m`);
        })
        .catch((error) => {
            throw error;
        });

    // Extract files in parallel.
    console.log("\x1b[34mExtracting downloaded files...\x1b[0m");
    await async.eachLimit(selectedFiles, 2, async (file: GitHubAsset) => {
        const uuid = crypto.randomUUID();
        const extractDir = `./server-dist/${uuid}`;
        if (!fs.existsSync(extractDir)) {
            fs.mkdirSync(extractDir);
        }

        const fileName = file.name;
        const filePath = `./server-dist/${fileName}`;
        if (fileName.endsWith(".zip") || fileName.endsWith(".tar.gz")) {
            await decompress(filePath, extractDir)
                .then((files) => {
                    console.log(
                        `\x1b[32m[OK]\x1b[0m \x1b[36m${fileName}\x1b[0m Extracted ${files.length} files.`
                    );
                })
                .catch((error) => {
                    console.log(`\x1b[31m[ERROR]\x1b[0m \x1b[36m${fileName}\x1b[0m ${error}`);
                    fs.rmSync(extractDir, { recursive: true });
                    throw error;
                });
        }

        const exe = fileName.includes("windows") ? ".exe" : "";

        const triplet = makeTripletFromFileName(fileName);
        fs.renameSync(`${extractDir}/memos${exe}`, `./server-dist/memos-${triplet}${exe}`);
        // chmod +x downloaded file.
        if (process.platform !== "win32") {
            fs.chmodSync(`./server-dist/memos-${triplet}${exe}`, 0o755);
        }

        // Check if there's a sidecar front-end folder (Memos v0.18.2 - v0.21.0).
        const sidecarDist = `${extractDir}/dist`;
        const frontendDist = "./server-dist/dist";
        if (fs.existsSync(sidecarDist)) {
            // Move front-end dist folder only once, as it's the same for all platforms.
            if (!fs.existsSync(frontendDist)) {
                fs.renameSync(sidecarDist, frontendDist);
            }
        }

        fs.rmSync(extractDir, { recursive: true });
        fs.rmSync(filePath);
    });
}

async function main() {
    const startTime = performance.now();

    const repoRoot = findRepositoryRoot();
    console.log(`Repository root is \`${repoRoot}\``);
    process.chdir(repoRoot);
    console.log("\x1b[47m\x1b[32m|> Running script `Download Memos Builds` <|\x1b[0m");

    const serverDistDir = "./server-dist";
    const serverDistDirExists =
        fs.existsSync(serverDistDir) && fs.statSync(serverDistDir).isDirectory();
    if (!serverDistDirExists) {
        fs.mkdirSync(serverDistDir, { recursive: true, mode: 0o755 });
    }

    const binaries = [
        "memos-aarch64-apple-darwin",
        "memos-x86_64-apple-darwin",
        "memos-x86_64-pc-windows-msvc.exe",
        "memos-x86_64-unknown-linux-gnu"
    ];
    const foundRecent = binaries.every((bin) => {
        if (!fs.existsSync(`${serverDistDir}/${bin}`)) {
            return false;
        }
        return (
            fs.statSync(`${serverDistDir}/${bin}`).ctimeMs >=
            Date.now() - 1000 * 60 * 60 * 24 * 7 // 7 days
        );
    });
    if (foundRecent) {
        console.log(`Found all required binaries: ${binaries}.`);
        console.log("Skipping download.");
        return;
    }

    // Remove a previous dist folder (Memos v0.18.2 - v0.21.0), if it exists.
    const distDir = "./server-dist/dist";
    if (serverDistDirExists && fs.existsSync(distDir) && fs.statSync(distDir).isDirectory()) {
        fs.rmSync(distDir, { force: true, recursive: true });
    }

    await downloadServerBinaries();

    const endTime = performance.now();
    const timeElapsed = endTime - startTime;
    console.log("Time elapsed: ", timeElapsed, "ms");
}

if (import.meta.main) {
    // This script will download about 52 MB of data for the main supported platforms
    // (aarch64-apple-darwin, x86_64-apple-darwin, x86_64-pc-windows-msvc and x86_64-unknown-linux-gnu).
    // It will take around 7 minutes on a slow 1 Mbps connection.
    const timeoutMinutes = 10;
    const wrapped = async.timeout(main, timeoutMinutes * 60 * 1000, "Script timed out.");
    wrapped((err?: Error | null, _data?: any) => {
        if (err) {
            throw err;
        }
    });
}
