import { test } from "node:test";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import assert from "node:assert/strict";
import {
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { prepareRelease } from "./prepare-release.mjs";

function fixture(version, notes = "Release notes\n") {
  const root = mkdtempSync(join(tmpdir(), "readio-release-"));
  for (const dir of ["crates/desk-app", "frontend", "docs/releases"])
    mkdirSync(join(root, dir), { recursive: true });
  const write = (path, data) => writeFileSync(join(root, path), data);
  write(
    "Cargo.toml",
    `[workspace.package]\nversion = "${version}"\n[workspace.dependencies]\n`,
  );
  write("crates/desk-app/tauri.conf.json", JSON.stringify({ version }));
  write("frontend/package.json", JSON.stringify({ version }));
  write(
    "frontend/package-lock.json",
    JSON.stringify({ version, packages: { "": { version } } }),
  );
  write(`docs/releases/v${version}.md`, notes);
  return root;
}

test("reads matching versions and release notes", (t) => {
  const root = fixture("0.2.5");
  t.after(() => rmSync(root, { recursive: true, force: true }));
  assert.deepEqual(prepareRelease(root, "v0.2.5"), {
    version: "0.2.5",
    body: "Release notes\n",
    prerelease: false,
  });
});

test("marks SemVer prereleases, but not build metadata, as prereleases", (t) => {
  for (const [version, expected] of [
    ["1.2.3-rc.1+build.7", true],
    ["1.2.3+build.7", false],
  ]) {
    const root = fixture(version);
    t.after(() => rmSync(root, { recursive: true, force: true }));
    assert.equal(prepareRelease(root, `v${version}`).prerelease, expected);
  }
});

test("rejects invalid tags, mismatched versions and missing or empty notes", (t) => {
  const root = fixture("0.2.5");
  t.after(() => rmSync(root, { recursive: true, force: true }));
  for (const tag of [
    "0.2.5",
    "v01.2.5",
    "v0.2.5-01",
    "v0.2.5/../../x",
    "v0.2.5\n",
  ]) {
    assert.throws(() => prepareRelease(root, tag));
  }
  assert.throws(() => prepareRelease(root, "v0.2.6"), /does not match/);
  rmSync(join(root, "docs/releases/v0.2.5.md"));
  assert.throws(() => prepareRelease(root, "v0.2.5"), /ENOENT/);
  writeFileSync(join(root, "docs/releases/v0.2.5.md"), " \n");
  assert.throws(() => prepareRelease(root, "v0.2.5"), /empty/);
});

test("reads a CRLF workspace section at the end of Cargo.toml", (t) => {
  const root = fixture("0.2.5");
  t.after(() => rmSync(root, { recursive: true, force: true }));
  writeFileSync(
    join(root, "Cargo.toml"),
    '[workspace]\r\nresolver = "2"\r\n\r\n[workspace.package]\r\nversion = "0.2.5"\r\n',
  );
  assert.equal(prepareRelease(root, "v0.2.5").version, "0.2.5");
});

test("CLI writes multiline outputs and leaves output untouched on version mismatch", (t) => {
  const root = fixture("0.2.5", "First line\nSecond line\n");
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const output = join(root, "github-output");
  writeFileSync(output, "");
  const run = (tag) =>
    spawnSync(
      process.execPath,
      [fileURLToPath(new URL("./prepare-release.mjs", import.meta.url))],
      {
        cwd: root,
        env: { ...process.env, TAG: tag, GITHUB_OUTPUT: output },
        encoding: "utf8",
      },
    );
  const result = run("v0.2.5");
  assert.equal(result.status, 0, result.stderr);
  const contents = readFileSync(output, "utf8");
  const match = contents.match(
    /^version=0\.2\.5\nprerelease=false\nbody<<(release_[\w-]+)\nFirst line\nSecond line\n\n\1\n$/,
  );
  assert.ok(match, contents);
  writeFileSync(output, "");
  const failed = run("v0.2.6");
  assert.notEqual(failed.status, 0);
  assert.match(failed.stderr, /does not match/);
  assert.equal(readFileSync(output, "utf8"), "");
});
