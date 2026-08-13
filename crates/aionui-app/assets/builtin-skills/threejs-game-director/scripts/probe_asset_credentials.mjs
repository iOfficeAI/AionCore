#!/usr/bin/env node
/**
 * Prints exactly one line per key in the form KEY=SET or KEY=MISSING.
 * The literal MISSING token is a contract: skill skip rules and
 * audit_reference_report.py blocker detection grep for it.
 *
 * Prefer this over probe_asset_credentials.sh on Windows (no bash required).
 * AionUi hydrates process.env from the user PATH before spawning aioncore.
 */

const KEYS = ["TRIPO_API_KEY", "GEMINI_API_KEY", "ELEVENLABS_API_KEY"];

for (const key of KEYS) {
  const value = (process.env[key] ?? "").trim();
  console.log(`${key}=${value ? "SET" : "MISSING"}`);
}
