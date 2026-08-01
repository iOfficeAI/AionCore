import { lstat, readdir, readFile, realpath, rename, rm, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import path from "node:path";

const LEGACY_PREFIX = "lark-";

function isOfficialLarkSkill(name, lockEntry) {
  if (lockEntry?.source === "larksuite/cli") {
    return true;
  }

  try {
    const sourceUrl = new URL(lockEntry?.sourceUrl);
    const isGitHubSource = sourceUrl.hostname === "github.com"
      && sourceUrl.pathname.replace(/\.git$/, "") === "/larksuite/cli";
    const isWellKnownSource = lockEntry?.source === "open.feishu.cn"
      && lockEntry?.sourceType === "well-known"
      && sourceUrl.protocol === "https:"
      && sourceUrl.hostname === "open.feishu.cn"
      && sourceUrl.port === ""
      && sourceUrl.username === ""
      && sourceUrl.password === ""
      && sourceUrl.pathname === `/.well-known/skills/${name}/SKILL.md`
      && sourceUrl.search === ""
      && sourceUrl.hash === "";
    return isGitHubSource || isWellKnownSource;
  } catch {
    return false;
  }
}

async function readLockfile(lockPath) {
  try {
    return { path: lockPath, lock: JSON.parse(await readFile(lockPath, "utf8")) };
  } catch (error) {
    if (error.code === "ENOENT") return null;
    throw new Error(`Cannot parse ${lockPath}: ${error.message}`);
  }
}

function officialEntries(lock) {
  return Object.entries(lock?.skills ?? {})
    .filter(([name, entry]) => name.startsWith(LEGACY_PREFIX) && isOfficialLarkSkill(name, entry))
    .map(([name]) => name)
    .sort();
}

async function childDirectories(directory) {
  try {
    return (await readdir(directory, { withFileTypes: true }))
      .filter((entry) => entry.isDirectory())
      .map((entry) => path.join(directory, entry.name));
  } catch {
    return [];
  }
}

async function globalSkillsDirectories(homeDirectory, canonicalSkillsDirectory) {
  const directories = new Set();

  async function visit(directory, depth) {
    if (depth === 0) return;
    for (const child of await childDirectories(directory)) {
      if (path.basename(child) === "skills") {
        directories.add(child);
      }
      await visit(child, depth - 1);
    }
  }

  for (const directory of await childDirectories(homeDirectory)) {
    if (path.basename(directory).startsWith(".")) await visit(directory, 3);
  }

  const canonicalDirectory = await realpath(canonicalSkillsDirectory).catch(() => canonicalSkillsDirectory);
  const aliasDirectories = [];
  for (const directory of directories) {
    const resolvedDirectory = await realpath(directory).catch(() => directory);
    if (resolvedDirectory !== canonicalDirectory) aliasDirectories.push(directory);
  }
  return aliasDirectories;
}

async function globalLinkedSkills(homeDirectory, canonicalSkillsDirectory, names) {
  const linkedSkills = [];
  const skillsDirectories = await globalSkillsDirectories(homeDirectory, canonicalSkillsDirectory);

  for (const skillsDirectory of skillsDirectories) {
    for (const name of names) {
      const candidate = path.join(skillsDirectory, name);
      try {
        if (!(await lstat(candidate)).isSymbolicLink()) continue;
        const [target, canonicalSkill] = await Promise.all([
          realpath(candidate),
          realpath(path.join(canonicalSkillsDirectory, name)),
        ]);
        if (target === canonicalSkill) linkedSkills.push(candidate);
      } catch {
        // Ignore missing or unresolved links. Only existing links to confirmed skills are removed.
      }
    }
  }

  return linkedSkills;
}

function installationPaths(projectRoot, global) {
  if (global) {
    const agentRoot = path.join(homedir(), ".agents");
    return {
      agentRoot,
      registryPaths: [process.env.XDG_STATE_HOME
        ? path.join(process.env.XDG_STATE_HOME, "skills", ".skill-lock.json")
        : path.join(agentRoot, ".skill-lock.json")],
    };
  }

  const resolvedProjectRoot = path.resolve(projectRoot);
  const agentRoot = path.join(resolvedProjectRoot, ".agents");
  return {
    agentRoot,
    registryPaths: [
      path.join(resolvedProjectRoot, "skills-lock.json"),
      path.join(agentRoot, ".skill-lock.json"),
    ],
  };
}

export async function inspectLegacyInstallation(projectRoot = process.cwd(), { global = false } = {}) {
  const { agentRoot, registryPaths } = installationPaths(projectRoot, global);
  const skillsDirectory = path.join(agentRoot, "skills");
  let directories = [];

  try {
    directories = (await readdir(skillsDirectory, { withFileTypes: true }))
      .filter((entry) => entry.isDirectory() && entry.name.startsWith(LEGACY_PREFIX))
      .map((entry) => entry.name)
      .sort();
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
  }

  const registries = (await Promise.all(registryPaths.map(readLockfile))).filter(Boolean).map(({ path: lockPath, lock }) => ({
    path: lockPath,
    lock,
    officialEntries: officialEntries(lock),
  }));
  const lockEntries = [...new Set(registries.flatMap((registry) => registry.officialEntries))].sort();
  const confirmedDirectories = directories.filter((name) => lockEntries.includes(name));
  const untrackedDirectories = directories.filter((name) => !lockEntries.includes(name));
  const linkedSkills = global
    ? await globalLinkedSkills(homedir(), skillsDirectory, confirmedDirectories)
    : [];

  return {
    agentRoot,
    skillsDirectory,
    registries,
    lockEntries,
    confirmedDirectories,
    linkedSkills,
    untrackedDirectories,
  };
}

export async function removeLegacyInstallation(projectRoot = process.cwd(), options) {
  const inspection = await inspectLegacyInstallation(projectRoot, options);
  await Promise.all(
    [
      ...inspection.confirmedDirectories.map((name) => path.join(inspection.skillsDirectory, name)),
      ...inspection.linkedSkills,
    ].map((skill) => rm(skill, { recursive: true, force: true })),
  );

  await Promise.all(inspection.registries.map(async (registry) => {
    if (registry.officialEntries.length === 0) return;
    for (const name of registry.officialEntries) delete registry.lock.skills[name];
    const temporaryPath = `${registry.path}.tmp`;
    await writeFile(temporaryPath, `${JSON.stringify(registry.lock, null, 2)}\n`);
    await rename(temporaryPath, registry.path);
  }));

  return inspection;
}
