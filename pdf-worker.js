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
      type: "ready",
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
        // --no-sandbox is required in Docker containers. The Dockerfile runs Chrome
        // as non-root (appuser), and Docker provides its own process isolation.
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
      type: "ready",
      ready: false,
      error: `Failed to launch Chrome: ${err.message}`,
    });
    process.exit(1);
  }

  // Track in-flight render IDs so we can send error responses on disconnect.
  const inFlightIds = new Set();

  // If Chrome disconnects unexpectedly, notify all in-flight renders and exit.
  browser.on("disconnected", () => {
    process.stderr.write("Chrome disconnected unexpectedly, exiting.\n");
    for (const id of inFlightIds) {
      sendResponse({ type: "response", id, success: false, error: "Chrome disconnected unexpectedly" });
    }
    inFlightIds.clear();
    process.exit(1);
  });

  sendResponse({ type: "ready", ready: true });

  // Concurrency is controlled by the Rust semaphore (max_concurrent_renders).
  // Each job gets its own Chrome page, so they can run in parallel.

  async function renderPdf(id, htmlPath, pdfPath) {
    inFlightIds.add(id);
    let page;
    try {
      const htmlContent = await fs.promises.readFile(htmlPath, "utf-8");

      page = await browser.newPage();
      page.setDefaultTimeout(PAGE_TIMEOUT_MS);
      page.setDefaultNavigationTimeout(PAGE_TIMEOUT_MS);

      await page.setRequestInterception(true);
      page.on('request', (req) => {
        const url = req.url();
        if (url.startsWith('data:image/') || url.startsWith('data:font/') || url === 'about:blank') {
          req.continue();
        } else {
          req.abort('blockedbyclient');
        }
      });

      await page.setContent(htmlContent, { waitUntil: "load", timeout: PAGE_TIMEOUT_MS });
      await page.pdf({
        path: pdfPath,
        format: "A4",
        printBackground: true,
        margin: { top: "1cm", right: "1cm", bottom: "1cm", left: "1cm" },
        timeout: PAGE_TIMEOUT_MS,
      });

      sendResponse({ type: "response", id, success: true });
    } catch (err) {
      sendResponse({ type: "response", id, success: false, error: err.message });
    } finally {
      inFlightIds.delete(id);
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

    const { id, htmlPath, pdfPath } = request;
    if (!id || !htmlPath || !pdfPath) {
      if (id) sendResponse({ type: "response", id, success: false, error: "Missing required fields" });
      return;
    }
    renderPdf(id, htmlPath, pdfPath).catch((err) => {
      sendResponse({ type: "response", id, success: false, error: err.message || "Unexpected error" });
    });
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
