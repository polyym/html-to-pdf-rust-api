// Syncs the package.json version from Cargo.toml so that Cargo.toml is the
// single source of truth. Runs automatically as an npm prebuild hook.
const fs = require("fs");

const cargo = fs.readFileSync("Cargo.toml", "utf8");
const match = cargo.match(/^version\s*=\s*"(.+?)"/m);

if (!match) {
  console.error("Could not find version in Cargo.toml");
  process.exit(1);
}

const version = match[1];
const pkg = JSON.parse(fs.readFileSync("package.json", "utf8"));

if (pkg.version !== version) {
  pkg.version = version;
  fs.writeFileSync("package.json", JSON.stringify(pkg, null, 2) + "\n");
  console.log(`Synced package.json version to ${version}`);
}
