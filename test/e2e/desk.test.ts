// afterglow --stdio's desktop, driven by Link as phosphorus drives it over ssh: what the
// build says it serves, the user's processes listed, one asked to end and one killed,
// afterglow itself refused, a file in the user's home moved to the trash and one
// deleted for good, one outside the home refused. Runs where AFTERGLOW_BIN names a
// built afterglow (`bun run test:e2e`); no display needed

import { expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Link, type Transport } from "../../src/index.ts";

const bin = process.env.AFTERGLOW_BIN ?? "";
const run = !!bin && existsSync(bin) && process.platform === "linux";

function spawnStdio(home: string) {
  const p = Bun.spawn([bin, "--stdio"], {
    stdin: "pipe",
    stdout: "pipe",
    stderr: "inherit",
    env: { ...process.env, DISPLAY: "", HOME: home, XDG_DATA_HOME: "" },
  });
  const datas: ((b: Uint8Array) => void)[] = [];
  const closes: (() => void)[] = [];
  void (async () => {
    for await (const c of p.stdout) for (const f of datas) f(c);
    for (const f of closes) f();
  })();
  const t: Transport = {
    write: (b) => {
      p.stdin.write(b);
      p.stdin.flush();
    },
    onData: (f) => void datas.push(f),
    onClose: (f) => void closes.push(f),
    close: () => {
      try {
        p.stdin.end();
      } catch {}
    },
  };
  return { p, t };
}

test.skipIf(!run)(
  "the desktop over --stdio: processes listed and ended, files trashed or deleted",
  async () => {
    const features = Bun.spawnSync([bin, "--features"]).stdout.toString();
    expect(features).toContain("processes delete");
    expect(features).not.toContain("attach");

    const home = mkdtempSync(join(tmpdir(), "afterglow-desk-home-"));
    const outside = mkdtempSync(join(tmpdir(), "afterglow-desk-out-"));
    mkdirSync(join(home, "Desktop"));
    writeFileSync(join(home, "Desktop", "note.txt"), "x");
    writeFileSync(join(home, "Desktop", "gone.txt"), "y");
    writeFileSync(join(outside, "keep.txt"), "z");
    const sleeper = Bun.spawn(["sleep", "60"]);
    const stubborn = Bun.spawn(["sh", "-c", "trap '' TERM; while :; do sleep 0.05; done"]);
    await Bun.sleep(200);
    const { p, t } = spawnStdio(home);
    const link = new Link(t, { message: () => {}, closed: () => {} });
    try {
      const ready = await link.open(10_000);
      expect(ready.features).toEqual(["processes", "delete"]);

      // nothing has a window on no display; all of them: the children among them
      expect(await link.processes()).toEqual([]);
      const all = await link.processes(true);
      const s = all.find((x) => x.pid === sleeper.pid);
      expect(s?.name).toBe("sleep");
      expect(s?.windows).toEqual([]);
      expect(all.some((x) => x.pid === p.pid)).toBe(true);

      // afterglow itself is never ended; a start time that isn't its own is another process
      await expect(link.end(p.pid, { kill: true })).rejects.toThrow("afterglow");
      await expect(link.end(sleeper.pid, { started: (s?.started ?? 0) + 5000 })).rejects.toThrow("another process");
      expect(await link.end(sleeper.pid, { started: s?.started })).toEqual({ how: "terminated", gone: true });
      expect(await link.end(stubborn.pid, { waitMs: 200 })).toEqual({ how: "terminated", gone: false });
      expect(await link.end(stubborn.pid, { kill: true })).toEqual({ how: "killed", gone: true });

      // to the trash in the user's home, as a file manager would; for good when asked
      expect(await link.delete(join(home, "Desktop", "note.txt"))).toEqual({ recycled: true });
      expect(existsSync(join(home, "Desktop", "note.txt"))).toBe(false);
      expect(readFileSync(join(home, ".local/share/Trash/files/note.txt"), "utf8")).toBe("x");
      expect(readFileSync(join(home, ".local/share/Trash/info/note.txt.trashinfo"), "utf8")).toContain(
        "/Desktop/note.txt",
      );
      expect(await link.delete(join(home, "Desktop", "gone.txt"), true)).toEqual({ recycled: false });
      expect(existsSync(join(home, "Desktop", "gone.txt"))).toBe(false);
      // outside the home, or the home itself: refused, left as it is
      await expect(link.delete(join(outside, "keep.txt"), true)).rejects.toThrow("outside");
      await expect(link.delete(home, true)).rejects.toThrow();
      expect(existsSync(join(outside, "keep.txt"))).toBe(true);
    } finally {
      link.close();
      sleeper.kill(9);
      stubborn.kill(9);
      await p.exited;
      rmSync(home, { recursive: true, force: true });
      rmSync(outside, { recursive: true, force: true });
    }
  },
  30_000,
);
