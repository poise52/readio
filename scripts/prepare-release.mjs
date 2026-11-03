import { appendFileSync, readFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const semver =
  /^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*))?(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$/;

export function prepareRelease(root, tag) {
  const match = semver.exec(tag);
  if (!match || match[0] !== tag)
    throw new Error("Release tag must be v followed by a valid SemVer version");
  const version = tag.slice(1);
  const read = (path) => readFileSync(join(root, path), "utf8");
  const lines = read("Cargo.toml").split(/\r?\n/);
  const sectionStart = lines.findIndex(
    (line) => line.trim() === "[workspace.package]",
  );
  const workspaceLines = sectionStart < 0 ? [] : lines.slice(sectionStart + 1);
  const sectionEnd = workspaceLines.findIndex((line) => /^\s*\[/.test(line));
  const packageLines =
    sectionEnd < 0 ? workspaceLines : workspaceLines.slice(0, sectionEnd);
  const workspaceVersion = packageLines
    .map((line) => line.match(/^\s*version\s*=\s*"([^"]+)"\s*$/)?.[1])
    .find(Boolean);
  const app = JSON.parse(read("crates/desk-app/tauri.conf.json"));
  const frontend = JSON.parse(read("frontend/package.json"));
  const lock = JSON.parse(read("frontend/package-lock.json"));
  for (const [name, actual] of Object.entries({
    workspace: workspaceVersion,
    app: app.version,
    frontend: frontend.version,
    "frontend lock": lock.version,
    "frontend lock root": lock.packages?.[""]?.version,
  })) {
    if (actual !== version)
      throw new Error(
        `${name} version ${actual ?? "(missing)"} does not match ${version}`,
      );
  }
  const body = read(`docs/releases/${tag}.md`);
  if (!body.trim()) throw new Error("Release notes are empty");
  return { version, body, prerelease: Boolean(match[4]) };
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  try {
    const { version, body, prerelease } = prepareRelease(
      process.cwd(),
      process.env.TAG ?? "",
    );
    const delimiter = `release_${randomUUID()}`;
    appendFileSync(
      process.env.GITHUB_OUTPUT,
      `version=${version}\nprerelease=${prerelease}\nbody<<${delimiter}\n${body}\n${delimiter}\n`,
    );
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
