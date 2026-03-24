const puppeteer = require("puppeteer-core");
const fs = require("fs");
const path = require("path");
const readline = require("readline");
const os = require("os");

const PAGE_TIMEOUT_MS = 25_000;

process.on("unhandledRejection", (err) => {
  process.stderr.write(`Unhandled rejection: ${err}\n`);
});

process.on("uncaughtException", (err) => {
  process.stderr.write(`Uncaught exception: ${err}\n`);
  process.exit(1);
});

function detectChromePath() {
  const envPath = process.env.CHROME_EXECUTABLE_PATH;
  if (envPath) return envPath;

  const platform = os.platform();
  const candidates =
    platform === "win32"
      ? [
          path.join(
            process.env.PROGRAMFILES || "",
            "Google/Chrome/Application/chrome.exe"
          ),
          path.join(
            process.env["PROGRAMFILES(X86)"] || "",
            "Google/Chrome/Application/chrome.exe"
          ),
          path.join(
            process.env.LOCALAPPDATA || "",
            "Google/Chrome/Application/chrome.exe"
          ),
        ]
      : platform === "darwin"
        ? [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
          ]
        : [
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
          ];

  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) return candidate;
  }

  return null;
}

function sendResponse(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

async function main() {
  const chromePath = detectChromePath();
  if (!chromePath) {
    sendResponse({
      ready: false,
      error:
        "Chrome executable not found. Set CHROME_EXECUTABLE_PATH environment variable.",
    });
    process.exit(1);
  }

  let browser;
  try {
    browser = await puppeteer.launch({
      executablePath: chromePath,
      headless: true,
      args: [
        "--no-sandbox",
        "--disable-setuid-sandbox",
        "--disable-dev-shm-usage",
        "--disable-extensions",
        "--disable-gpu",
        "--disable-background-networking",
        "--disable-default-apps",
        "--disable-sync",
        "--disable-translate",
        "--metrics-recording-only",
        "--no-first-run",
      ],
    });
  } catch (err) {
    sendResponse({
      ready: false,
      error: `Failed to launch Chrome: ${err.message}`,
    });
    process.exit(1);
  }

  // If Chrome disconnects unexpectedly, exit so Rust can respawn us.
  browser.on("disconnected", () => {
    process.stderr.write("Chrome disconnected unexpectedly, exiting.\n");
    process.exit(1);
  });

  sendResponse({ ready: true });

  // --- Sequential job queue ---
  // readline fires "line" events synchronously, but our handler is async.
  // Queue jobs and process one at a time to avoid interleaving Chrome operations.
  const jobQueue = [];
  let processing = false;

  async function processQueue() {
    if (processing) return;
    processing = true;

    while (jobQueue.length > 0) {
      const { id, htmlPath, pdfPath } = jobQueue.shift();
      await renderPdf(id, htmlPath, pdfPath);
    }

    processing = false;
  }

  async function renderPdf(id, htmlPath, pdfPath) {
    let page;
    try {
      const htmlContent = fs.readFileSync(htmlPath, "utf-8");

      page = await browser.newPage();
      page.setDefaultTimeout(PAGE_TIMEOUT_MS);
      page.setDefaultNavigationTimeout(PAGE_TIMEOUT_MS);

      await page.setContent(htmlContent, { waitUntil: "networkidle0", timeout: PAGE_TIMEOUT_MS });
      await page.pdf({
        path: pdfPath,
        format: "A4",
        printBackground: true,
        margin: { top: "1cm", right: "1cm", bottom: "1cm", left: "1cm" },
        timeout: PAGE_TIMEOUT_MS,
      });

      sendResponse({ id, success: true });
    } catch (err) {
      sendResponse({ id, success: false, error: err.message });
    } finally {
      if (page) {
        try {
          await page.close();
        } catch {
          // Page may already be closed if Chrome disconnected.
        }
      }
    }
  }

  const rl = readline.createInterface({ input: process.stdin });

  rl.on("line", (line) => {
    let request;
    try {
      request = JSON.parse(line);
    } catch {
      return;
    }

    jobQueue.push(request);
    processQueue();
  });

  rl.on("close", async () => {
    if (browser) {
      try {
        await browser.close();
      } catch {
        // Ignore close errors during shutdown.
      }
    }
    process.exit(0);
  });
}

main();
