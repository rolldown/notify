#!/usr/bin/env node

import { readFileSync } from "node:fs";
import { join } from "node:path";
import { setTimeout } from "node:timers/promises";

async function waitForPackage(packageDir) {
  const { name, version } = JSON.parse(readFileSync(join(packageDir, "package.json"), "utf8"));
  const spec = `${name}@${version}`;
  const metadataUrl = `https://registry.npmjs.org/${encodeURIComponent(name)}/${encodeURIComponent(version)}`;
  const deadline = Date.now() + 24 * 60 * 60 * 1000;

  while (Date.now() < deadline) {
    try {
      const metadata = await fetch(metadataUrl, {
        headers: { "cache-control": "no-cache" },
        signal: AbortSignal.timeout(30_000),
      });
      if (!metadata.ok) throw new Error();

      const tarball = await fetch((await metadata.json()).dist.tarball, {
        headers: { "cache-control": "no-cache", range: "bytes=0-0" },
        signal: AbortSignal.timeout(30_000),
      });
      await tarball.body?.cancel();
      if (!tarball.ok) throw new Error();

      console.log(`✓ ${spec} is available.`);
      return;
    } catch {
      await setTimeout(10_000);
    }
  }

  throw new Error(`Timed out waiting for ${spec}.`);
}

Promise.all(process.argv.slice(2).map(waitForPackage)).catch((error) => {
  console.error(`::error::${error.message}`);
  process.exit(1);
});
