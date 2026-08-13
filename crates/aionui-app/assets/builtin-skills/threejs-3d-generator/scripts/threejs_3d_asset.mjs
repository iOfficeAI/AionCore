#!/usr/bin/env node
/**
 * Tripo OpenAPI 3D asset helper.
 * Prefer this Node script. python3 threejs_3d_asset.py is fallback only
 * when node is missing or this process exits non-zero.
 * Never run bare `python` (Windows Store stub).
 */

import fs from "node:fs";
import path from "node:path";
import {
  clipNameFromPreset,
  findDownloadedModel,
  parseAnimationList,
  retargetJobs,
} from "./character_pipeline_lib.mjs";

const BASE_URL = "https://api.tripo3d.ai/v2/openapi";
const FINAL_STATUSES = new Set(["success", "failed", "banned", "expired", "cancelled", "unknown"]);
const DOWNLOAD_KEYS = ["pbr_model", "model", "base_model", "rendered_image", "generated_image"];
const RIG_MODEL_VERSION = "v2.5-20260210";
const BIPED_PRESETS = [
  "preset:idle",
  "preset:walk",
  "preset:run",
  "preset:dive",
  "preset:climb",
  "preset:jump",
  "preset:slash",
  "preset:shoot",
  "preset:hurt",
  "preset:fall",
  "preset:turn",
];
const RIG_TYPE_PRESETS = {
  biped: new Set(BIPED_PRESETS),
  quadruped: new Set(["preset:quadruped:walk"]),
  hexapod: new Set(["preset:hexapod:walk"]),
  octopod: new Set(["preset:octopod:walk"]),
  serpentine: new Set(["preset:serpentine:march"]),
  aquatic: new Set(["preset:aquatic:march"]),
  avian: new Set(),
};
const KNOWN_PRESETS = new Set(Object.values(RIG_TYPE_PRESETS).flatMap((set) => [...set]));
const POST_TYPE_ALIASES = {
  convert_model: "conversion",
  conversion: "conversion",
  retarget: "animate_retarget",
  rig: "animate_rig",
  prerig: "animate_prerigcheck",
  prerigcheck: "animate_prerigcheck",
  lowpoly: "highpoly_to_lowpoly",
};

class TripoError extends Error {}

function eprint(...parts) {
  console.error(parts.join(" "));
}

function parseArgs(argv) {
  const args = { _: [] };
  for (let i = 0; i < argv.length; i += 1) {
    const token = argv[i];
    if (!token.startsWith("--")) {
      args._.push(token);
      continue;
    }
    const key = token.slice(2).replace(/-([a-z])/g, (_, ch) => ch.toUpperCase());
    const next = argv[i + 1];
    if (next === undefined || next.startsWith("--")) {
      args[key] = true;
    } else {
      args[key] = next;
      i += 1;
    }
  }
  return args;
}

function apiKeyFrom(args) {
  const key = args.apiKey || process.env.TRIPO_API_KEY;
  if (!key) {
    throw new TripoError("Missing API key. Set TRIPO_API_KEY or pass --api-key.");
  }
  return key;
}

async function jsonRequest(apiKey, method, apiPath, payload) {
  const response = await fetch(`${BASE_URL}${apiPath}`, {
    method,
    headers: {
      Authorization: `Bearer ${apiKey}`,
      ...(payload ? { "Content-Type": "application/json" } : {}),
    },
    body: payload ? JSON.stringify(payload) : undefined,
  });
  const raw = await response.text();
  let data;
  try {
    data = JSON.parse(raw);
  } catch {
    throw new TripoError(`HTTP ${response.status}: ${raw}`);
  }
  if (!response.ok) {
    throw new TripoError(`HTTP ${response.status}: ${raw}`);
  }
  if (data.code !== 0) {
    throw new TripoError(JSON.stringify(data, null, 2));
  }
  return data;
}

async function multipartUpload(apiKey, filePath) {
  if (!fs.existsSync(filePath)) {
    throw new TripoError(`Image not found: ${filePath}`);
  }
  const stat = fs.statSync(filePath);
  if (stat.size > 20 * 1024 * 1024) {
    throw new TripoError("Tripo upload limit is 20MB.");
  }
  const ext = path.extname(filePath).toLowerCase().replace(".", "").replace("jpg", "jpeg");
  if (!["png", "jpeg", "webp"].includes(ext)) {
    throw new TripoError("Direct image upload accepts png, jpeg/jpg, or webp.");
  }
  const form = new FormData();
  form.append("file", new Blob([fs.readFileSync(filePath)]), path.basename(filePath));
  const response = await fetch(`${BASE_URL}/upload/sts`, {
    method: "POST",
    headers: { Authorization: `Bearer ${apiKey}` },
    body: form,
  });
  const raw = await response.text();
  const data = JSON.parse(raw);
  if (data.code !== 0) {
    throw new TripoError(raw);
  }
  const token = data?.data?.image_token;
  if (!token) {
    throw new TripoError(`Upload response did not include image_token: ${raw}`);
  }
  return token;
}

async function submitTask(apiKey, payload) {
  const data = await jsonRequest(apiKey, "POST", "/task", payload);
  const taskId = data?.data?.task_id;
  if (!taskId) {
    throw new TripoError(`Task response did not include task_id: ${JSON.stringify(data)}`);
  }
  console.log(taskId);
  return taskId;
}

async function getTask(apiKey, taskId) {
  return (await jsonRequest(apiKey, "GET", `/task/${taskId}`)).data;
}

async function waitForTask(apiKey, taskId, interval = 8, timeout = 600) {
  const started = Date.now();
  for (;;) {
    const data = await getTask(apiKey, taskId);
    const status = data.status || "unknown";
    eprint(`${taskId}: ${status} ${data.progress || 0}%`);
    if (FINAL_STATUSES.has(status)) return data;
    if ((Date.now() - started) / 1000 > Number(timeout)) {
      throw new TripoError(`Timed out waiting for task ${taskId}`);
    }
    await new Promise((resolve) => setTimeout(resolve, Number(interval) * 1000));
  }
}

function extensionFor(key, url, contentType) {
  try {
    const ext = path.extname(new URL(url).pathname);
    if (ext) return ext;
  } catch {
    /* ignore */
  }
  if (contentType?.includes("png")) return ".png";
  if (contentType?.includes("jpeg")) return ".jpg";
  if (key.includes("image")) return ".png";
  return ".glb";
}

async function downloadUrl(url, outDir, filenameBase, key) {
  const response = await fetch(url);
  const buffer = Buffer.from(await response.arrayBuffer());
  const ext = extensionFor(key, url, response.headers.get("content-type"));
  fs.mkdirSync(outDir, { recursive: true });
  const filePath = path.join(outDir, `${filenameBase}-${key}${ext}`);
  fs.writeFileSync(filePath, buffer);
  return filePath;
}

async function downloadOutputs(task, outDir) {
  const taskId = task.task_id;
  const output = task.output || {};
  fs.mkdirSync(outDir, { recursive: true });
  fs.writeFileSync(path.join(outDir, `${taskId}.json`), JSON.stringify(task, null, 2));
  const paths = [];
  for (const key of DOWNLOAD_KEYS) {
    const url = output[key];
    if (typeof url === "string" && url.startsWith("http")) {
      const filePath = await downloadUrl(url, outDir, taskId, key);
      paths.push(filePath);
      console.log(filePath);
    }
  }
  const multiview = output.generate_multiview_image;
  if (multiview && typeof multiview === "object") {
    for (const [key, url] of Object.entries(multiview)) {
      if (typeof url === "string" && url.startsWith("http")) {
        const filePath = await downloadUrl(url, outDir, taskId, key);
        paths.push(filePath);
        console.log(filePath);
      }
    }
  }
  if (paths.length === 0) eprint("No downloadable output URLs found.");
  return paths;
}

function applyCommonModelArgs(args, payload) {
  const mapping = {
    negative_prompt: args.negativePrompt,
    model_seed: args.modelSeed != null ? Number(args.modelSeed) : undefined,
    image_seed: args.imageSeed != null ? Number(args.imageSeed) : undefined,
    texture_seed: args.textureSeed != null ? Number(args.textureSeed) : undefined,
    texture_quality: args.textureQuality,
    geometry_quality: args.geometryQuality,
    face_limit: args.faceLimit != null ? Number(args.faceLimit) : undefined,
    smart_low_poly: args.smartLowPoly || undefined,
    quad: args.quad || undefined,
    auto_size: args.autoSize || undefined,
    compress: args.compress,
    generate_parts: args.generateParts || undefined,
  };
  for (const [key, value] of Object.entries(mapping)) {
    if (value !== undefined && value !== false) payload[key] = value;
  }
  if (args.noTexture) payload.texture = false;
  if (args.noPbr) payload.pbr = false;
  if (args.noExportUv) payload.export_uv = false;
}

async function maybeWaitAndDownload(apiKey, taskId, args) {
  if (!args.wait) return null;
  const task = await waitForTask(apiKey, taskId, args.interval, args.timeout);
  console.log(JSON.stringify(task, null, 2));
  if (task.status !== "success") {
    throw new TripoError(`Task ${taskId} ended as ${task.status}`);
  }
  if (args.download) await downloadOutputs(task, args.outDir || "tripo-output");
  return task;
}

function glbNodeNames(filePath) {
  const buf = fs.readFileSync(filePath);
  if (buf.length < 20 || buf.slice(0, 4).toString() !== "glTF") {
    throw new TripoError(`Not a GLB file: ${filePath}`);
  }
  const chunkLen = buf.readUInt32LE(12);
  const json = JSON.parse(buf.slice(20, 20 + chunkLen).toString("utf8"));
  return (json.nodes || []).map((node) => node.name || "").filter(Boolean);
}

function validateRigGlb(filePath, rigType) {
  const names = glbNodeNames(filePath);
  const bones = names.filter((name) => name.startsWith("tripo::"));
  const problems = [];
  if (bones.length) {
    const rows = {};
    for (const bone of bones) {
      if (!bone.includes("_Limb_")) continue;
      const parts = bone.split("::").at(-1).split("_");
      const row = parts[0];
      const side = parts[1];
      rows[row] ||= {};
      rows[row][side] = (rows[row][side] || 0) + 1;
    }
    for (const [row, sides] of Object.entries(rows)) {
      const keys = Object.keys(sides);
      if (!(keys.includes("Left") && keys.includes("Right"))) {
        problems.push(`limb row ${row} has only ${keys.join("/")} (asymmetric rig)`);
      }
    }
    if (bones.length < 12) problems.push(`suspiciously small skeleton (${bones.length} bones)`);
    return { description: `${bones.length} tripo:: bones`, problems };
  }
  const left = new Set(names.filter((name) => name.startsWith("L_")).map((name) => name.slice(2)));
  const right = new Set(names.filter((name) => name.startsWith("R_")).map((name) => name.slice(2)));
  if (left.size === 0 && right.size === 0) {
    return { description: "no recognizable rig bones", problems: ["no tripo:: or legacy L_/R_ bones found in rig GLB"] };
  }
  for (const part of ["Clavicle", "Upperarm", "Forearm", "Hand", "Thigh", "Calf", "Foot"]) {
    if (!left.has(part) || !right.has(part)) problems.push(`legacy rig missing L_/R_ ${part}`);
  }
  if (rigType && rigType !== "biped") {
    problems.push(`legacy anatomical skeleton is biped-only; requested rig_type ${rigType}`);
  }
  return { description: `legacy anatomical skeleton, ${left.size} paired L/R bones`, problems };
}

async function cmdText(args) {
  const apiKey = apiKeyFrom(args);
  const payload = {
    type: "text_to_model",
    prompt: args.prompt,
    model_version: args.modelVersion || "v3.1-20260211",
  };
  applyCommonModelArgs(args, payload);
  const taskId = await submitTask(apiKey, payload);
  await maybeWaitAndDownload(apiKey, taskId, args);
}

async function cmdImage(args) {
  const apiKey = apiKeyFrom(args);
  let fileObj;
  if (String(args.image).startsWith("http://") || String(args.image).startsWith("https://")) {
    fileObj = { type: "image", url: args.image };
  } else {
    fileObj = { type: "image", file_token: await multipartUpload(apiKey, args.image) };
  }
  const payload = {
    type: "image_to_model",
    file: fileObj,
    model_version: args.modelVersion || "v3.1-20260211",
  };
  if (args.enableImageAutofix) payload.enable_image_autofix = true;
  if (args.textureAlignment) payload.texture_alignment = args.textureAlignment;
  if (args.orientation) payload.orientation = args.orientation;
  applyCommonModelArgs(args, payload);
  const taskId = await submitTask(apiKey, payload);
  await maybeWaitAndDownload(apiKey, taskId, args);
}

async function cmdStatus(args) {
  const apiKey = apiKeyFrom(args);
  const taskId = args._[1];
  if (!taskId) throw new TripoError("status requires TASK_ID");
  console.log(JSON.stringify(await getTask(apiKey, taskId), null, 2));
}

async function cmdDownload(args) {
  const apiKey = apiKeyFrom(args);
  const taskId = args._[1];
  if (!taskId) throw new TripoError("download requires TASK_ID");
  const task = await getTask(apiKey, taskId);
  if (task.status !== "success") {
    throw new TripoError(`Task is ${task.status}; download URLs are available after success.`);
  }
  await downloadOutputs(task, args.outDir || "tripo-output");
}

async function cmdPostprocess(args) {
  const apiKey = apiKeyFrom(args);
  const taskType = POST_TYPE_ALIASES[args.type] || args.type;
  const payload = {
    type: taskType,
    original_model_task_id: args.originalTaskId,
  };
  if (args.modelVersion) payload.model_version = args.modelVersion;
  if (args.texturePrompt) payload.texture_prompt = args.texturePrompt;
  if (args.textureQuality) payload.texture_quality = args.textureQuality;
  if (args.outFormat) payload.out_format = args.outFormat;
  if (args.rigType) payload.rig_type = args.rigType;
  if (args.spec) payload.spec = args.spec;
  if (args.animation) payload.animation = args.animation;
  if (args.animations) payload.animations = String(args.animations).split(",").map((item) => item.trim()).filter(Boolean);
  if (args.animateInPlace) payload.animate_in_place = true;
  if (args.noBakeAnimation) payload.bake_animation = false;
  if (args.noExportWithGeometry) payload.export_with_geometry = false;
  if (args.format) payload.format = args.format;
  if (args.faceLimit) payload.face_limit = Number(args.faceLimit);
  if (args.textureSize) payload.texture_size = Number(args.textureSize);
  if (args.quad) payload.quad = true;
  if (args.forceSymmetry) payload.force_symmetry = true;
  if (args.flattenBottom) payload.flatten_bottom = true;
  if (args.flattenBottomThreshold) payload.flatten_bottom_threshold = Number(args.flattenBottomThreshold);
  if (args.style) payload.style = args.style;
  if (args.blockSize) payload.block_size = Number(args.blockSize);
  if (taskType === "animate_rig" && args.rigType === "biped" && !args.modelVersion) {
    payload.model_version = "v1.0-20240301";
  } else if (taskType === "animate_rig" && args.rigType && args.rigType !== "biped" && !args.modelVersion) {
    payload.model_version = RIG_MODEL_VERSION;
  }
  const taskId = await submitTask(apiKey, payload);
  await maybeWaitAndDownload(apiKey, taskId, args);
}

async function generateBaseModel(apiKey, args, outDir) {
  if (args.image) {
    let fileObj;
    if (String(args.image).startsWith("http://") || String(args.image).startsWith("https://")) {
      fileObj = { type: "image", url: args.image };
    } else {
      fileObj = { type: "image", file_token: await multipartUpload(apiKey, args.image) };
    }
    const payload = {
      type: "image_to_model",
      file: fileObj,
      model_version: args.modelVersion || "v3.1-20260211",
      texture_quality: args.textureQuality || "detailed",
      geometry_quality: args.geometryQuality || "standard",
      pbr: true,
    };
    if (args.enableImageAutofix) payload.enable_image_autofix = true;
    if (args.textureAlignment) payload.texture_alignment = args.textureAlignment;
    if (args.faceLimit) payload.face_limit = Number(args.faceLimit);
    const modelTaskId = await submitTask(apiKey, payload);
    const modelTask = await waitForTask(apiKey, modelTaskId, args.interval || 8, args.timeout || 900);
    if (modelTask.status !== "success") throw new TripoError(`Model task failed: ${modelTask.status}`);
    await downloadOutputs(modelTask, path.join(outDir, "base"));
    return modelTaskId;
  }
  if (!args.prompt) {
    throw new TripoError("--prompt or --image is required unless --model-task-id reuses an existing generation task");
  }
  let prompt = args.prompt;
  if ((args.rigType == null || args.rigType === "biped") && !/t-pose|a-pose/i.test(prompt)) {
    prompt = `${prompt}, full-body T-pose for rigging, arms straight out to the sides, legs apart and visible, front facing, symmetric, no props attached to the body`;
  }
  const payload = {
    type: "text_to_model",
    prompt,
    model_version: args.modelVersion || "v3.1-20260211",
    texture_quality: args.textureQuality || "detailed",
    geometry_quality: args.geometryQuality || "standard",
    pbr: true,
  };
  if (args.faceLimit) payload.face_limit = Number(args.faceLimit);
  const modelTaskId = await submitTask(apiKey, payload);
  const modelTask = await waitForTask(apiKey, modelTaskId, args.interval || 8, args.timeout || 900);
  if (modelTask.status !== "success") throw new TripoError(`Model task failed: ${modelTask.status}`);
  await downloadOutputs(modelTask, path.join(outDir, "base"));
  return modelTaskId;
}

async function cmdCharacterPipeline(args) {
  const apiKey = apiKeyFrom(args);
  if (!args.modelTaskId && !args.prompt && !args.image) {
    throw new TripoError("--prompt or --image is required unless --model-task-id reuses an existing generation task");
  }
  const outDir = args.outDir || "tripo-character";
  const interval = args.interval || 8;
  const timeout = args.timeout || 900;
  let modelTaskId = args.modelTaskId;
  if (!modelTaskId) {
    modelTaskId = await generateBaseModel(apiKey, args, outDir);
  }
  const checkId = await submitTask(apiKey, { type: "animate_prerigcheck", original_model_task_id: modelTaskId });
  const checkTask = await waitForTask(apiKey, checkId, interval, timeout);
  console.log(JSON.stringify(checkTask, null, 2));
  const rigType = args.rigType || checkTask.output?.rig_type || "biped";
  const rigPayload = {
    type: "animate_rig",
    original_model_task_id: modelTaskId,
    rig_type: rigType,
    spec: args.spec || "tripo",
    model_version: rigType === "biped" ? "v1.0-20240301" : RIG_MODEL_VERSION,
  };
  const rigId = await submitTask(apiKey, rigPayload);
  const rigTask = await waitForTask(apiKey, rigId, interval, timeout);
  if (rigTask.status !== "success") throw new TripoError(`Rig task failed: ${rigTask.status}`);
  const rigPaths = await downloadOutputs(rigTask, path.join(outDir, "rig"));
  const rigGlb = rigPaths.find((file) => file.endsWith(".glb"));
  if (rigGlb) {
    const { description, problems } = validateRigGlb(rigGlb, rigType);
    console.log(description);
    if (problems.length) throw new TripoError(`Rig validation failed: ${problems.join("; ")}`);
    console.log("Rig looks structurally valid.");
  }
  const animations = parseAnimationList(args.animations);
  if (animations.length === 0) return;
  if (args.spec === "mixamo") {
    throw new TripoError("spec=mixamo rigs cannot be used with Tripo animate_retarget.");
  }
  for (const animation of animations) {
    if (animation.startsWith("preset:") && !animation.startsWith("preset:biped:") && !KNOWN_PRESETS.has(animation)) {
      throw new TripoError(`Unknown animation preset ${animation}`);
    }
  }
  const jobs = retargetJobs(rigType, animations, {
    rigModelVersion: rigType === "biped" ? "v1.0-20240301" : RIG_MODEL_VERSION,
  });
  for (const job of jobs) {
    const retargetPayload = {
      type: job.type,
      original_model_task_id: rigId,
      out_format: job.out_format,
    };
    if (job.animation) retargetPayload.animation = job.animation;
    if (job.animations) retargetPayload.animations = job.animations;
    if (job.model_version) retargetPayload.model_version = job.model_version;
    const retargetId = await submitTask(apiKey, retargetPayload);
    const retargetTask = await waitForTask(apiKey, retargetId, interval, timeout);
    if (retargetTask.status !== "success") throw new TripoError(`Retarget failed: ${retargetTask.status}`);
    const label = clipNameFromPreset(job.animation || job.animations?.[0] || "clip");
    await downloadOutputs(retargetTask, path.join(outDir, "animated", label));
  }
}

function copyCastFile(source, dest) {
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  fs.copyFileSync(source, dest);
}

function installCastLook(gameDir, sources) {
  const lookDir = path.join(gameDir, "public", "look");
  fs.mkdirSync(lookDir, { recursive: true });
  const kitPath = path.join(lookDir, "look.json");
  const session = fs.existsSync(kitPath) ? JSON.parse(fs.readFileSync(kitPath, "utf8")) : {};
  session.models = { ...(session.models || {}) };

  const copyRole = (role, files) => {
    if (!files?.file) return;
    const destName = `${role}${path.extname(files.file) || ".glb"}`;
    copyCastFile(files.file, path.join(lookDir, destName));
    const slot = { file: `look/${destName}`, height: 1.7 };
    if (files.walk) {
      const walkName = `${role}-walk${path.extname(files.walk)}`;
      copyCastFile(files.walk, path.join(lookDir, walkName));
      slot.walk = `look/${walkName}`;
    }
    if (files.run) {
      const runName = `${role}-run${path.extname(files.run)}`;
      copyCastFile(files.run, path.join(lookDir, runName));
      slot.run = `look/${runName}`;
    }
    session.models[role] = slot;
  };

  copyRole("player", sources.player);
  copyRole("enemy", sources.enemy);
  if (sources.pickup?.file) {
    const destName = `pickup${path.extname(sources.pickup.file) || ".glb"}`;
    copyCastFile(sources.pickup.file, path.join(lookDir, destName));
    session.models.pickup = { file: `look/${destName}` };
  }
  fs.writeFileSync(kitPath, `${JSON.stringify(session, null, 2)}\n`);
  return kitPath;
}

async function cmdCast(args) {
  if (!args.out) throw new TripoError("cast requires --out <game-dir>");
  if (
    !args.playerImage &&
    !args.playerPrompt &&
    !args.enemyImage &&
    !args.enemyPrompt &&
    !args.pickupImage &&
    !args.pickupPrompt
  ) {
    throw new TripoError("cast requires --player-image/--enemy-image/--pickup-image (or matching --*-prompt)");
  }
  const gameDir = path.resolve(args.out);
  const tmp = path.join(gameDir, "tripo-cast");
  const sources = {};

  const runCharacter = async (role, image, prompt) => {
    if (!image && !prompt) return;
    const roleDir = path.join(tmp, role);
    await cmdCharacterPipeline({
      ...args,
      image,
      prompt,
      modelTaskId: undefined,
      outDir: roleDir,
      animations: args.animations || "preset:idle,preset:walk,preset:run",
    });
    sources[role] = {
      file:
        findDownloadedModel(path.join(roleDir, "animated", "idle")) ||
        findDownloadedModel(path.join(roleDir, "animated")),
      walk: findDownloadedModel(path.join(roleDir, "animated", "walk")),
      run: findDownloadedModel(path.join(roleDir, "animated", "run")),
    };
  };

  await runCharacter("player", args.playerImage, args.playerPrompt);
  await runCharacter("enemy", args.enemyImage, args.enemyPrompt);

  if (args.pickupImage || args.pickupPrompt) {
    const pickupDir = path.join(tmp, "pickup");
    if (args.pickupImage) {
      await cmdImage({ ...args, image: args.pickupImage, wait: true, download: true, outDir: pickupDir });
    } else {
      await cmdText({ ...args, prompt: args.pickupPrompt, wait: true, download: true, outDir: pickupDir });
    }
    sources.pickup = { file: findDownloadedModel(pickupDir) };
  }

  const kitPath = installCastLook(gameDir, sources);
  console.log(`CAST_OK path=${kitPath}`);
}

async function main(argv) {
  const args = parseArgs(argv);
  const command = args._[0];
  if (command === "probe") {
    console.log(`TRIPO_API_KEY=${process.env.TRIPO_API_KEY ? "SET" : "MISSING"}`);
    return;
  }
  if (command === "text") return cmdText(args);
  if (command === "image") return cmdImage(args);
  if (command === "status") return cmdStatus(args);
  if (command === "download") return cmdDownload(args);
  if (command === "postprocess") return cmdPostprocess(args);
  if (command === "character-pipeline") return cmdCharacterPipeline(args);
  if (command === "cast") return cmdCast(args);
  if (command === "validate-rig") {
    const glbPath = args._[1];
    if (!glbPath) throw new TripoError("validate-rig requires a GLB path");
    const { description, problems } = validateRigGlb(glbPath, args.rigType || "biped");
    console.log(description);
    if (problems.length) throw new TripoError(`Rig validation failed: ${problems.join("; ")}`);
    console.log("Rig looks structurally valid.");
    return;
  }
  if (command === "validate-animation") {
    const glbPath = args._[1];
    if (!glbPath) throw new TripoError("validate-animation requires a GLB path");
    const names = glbNodeNames(glbPath);
    console.log(`GLB nodes: ${names.length}`);
    return;
  }
  throw new TripoError(
    "Usage: threejs_3d_asset.mjs <probe|text|image|status|download|postprocess|character-pipeline|cast|validate-rig|validate-animation> [options]"
  );
}

main(process.argv.slice(2)).catch((error) => {
  eprint(error instanceof Error ? error.message : String(error));
  process.exit(1);
});
