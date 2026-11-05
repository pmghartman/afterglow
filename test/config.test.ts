// afterglow's files as a host app writes them: the config with the link on and
// GameStream's servers off, the token kept across starts, the driver's version, the
// link file read back

import { expect, test } from "bun:test";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  appsText,
  confText,
  driverVer,
  linkPort,
  linkText,
  pathsOf,
  readLink,
  svcBinPath,
  tokenOf,
} from "../src/index.ts";

test("the config: the link on, GameStream off, the encoders' fastest presets", () => {
  const p = pathsOf("C:\\Users\\me\\phosphorus\\state\\data\\afterglow");
  const conf = confText(p, linkPort, "ab".repeat(24));
  expect(conf.endsWith("\r\n")).toBe(true);
  const kv = Object.fromEntries(
    conf
      .split("\r\n")
      .filter((l) => l && !l.startsWith("#"))
      .map((l) => l.split(" = ")),
  );
  expect(kv.phos_link_port).toBe("47100");
  expect(kv.phos_link_token).toBe("ab".repeat(24));
  expect(kv.system_tray).toBe("disabled");
  expect(kv.upnp).toBe("off");
  expect(kv.file_apps).toBe(p.apps);
  expect(kv.dd_configuration_option).toBe("ensure_primary");
  expect(JSON.parse(appsText())).toEqual({ env: {}, apps: [] });
  expect(svcBinPath("C:\\h\\afterglow", "C:\\d\\afterglow.conf")).toBe(
    `"${join("C:\\h\\afterglow", "tools", "afterglowsvc.exe")}" "C:\\d\\afterglow.conf"`,
  );
});

test("the token: kept from a good link file, fresh otherwise", () => {
  const t = "0123456789abcdef".repeat(3);
  expect(JSON.parse(linkText(47100, "t"))).toEqual({ port: 47100, token: "t" });
  expect(tokenOf(linkText(47100, t), () => "fresh")).toBe(t);
  expect(tokenOf("not json", () => "fresh")).toBe("fresh");
  expect(tokenOf(linkText(47100, "short"), () => "fresh")).toBe("fresh");
});

test("the driver's version out of its INF", () => {
  expect(driverVer("[Version]\r\nDriverVer = 07/14/2025,1.10.9.289\r\n")).toBe("07/14/2025,1.10.9.289");
  expect(driverVer("[Version]\r\n")).toBe("");
});

test("the link file read back, nothing when it's missing or bad", () => {
  const d = mkdtempSync(join(tmpdir(), "afterglow-"));
  const p = pathsOf(d);
  expect(readLink(p.link)).toBeUndefined();
  const t = "0123456789abcdef".repeat(2);
  writeFileSync(p.link, linkText(linkPort, t));
  expect(readLink(p.link)).toEqual({ port: linkPort, token: t });
  writeFileSync(p.link, linkText(70000, t));
  expect(readLink(p.link)).toBeUndefined();
  writeFileSync(p.link, linkText(linkPort, "short"));
  expect(readLink(p.link)).toBeUndefined();
});
