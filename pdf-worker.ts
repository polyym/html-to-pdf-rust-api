import puppeteer, { Browser, Page, HTTPRequest, PaperFormat } from "puppeteer-core";
import fs from "fs";
import path from "path";
import readline from "readline";
import os from "os";

// --- IPC types (must mirror Rust WorkerRequest / WorkerMessage in src/worker.rs) ---

/** Render request received from the Rust server via stdin. */
interface WorkerRequest {
  id: string;
  htmlPath: string;
  pdfPath: string;
  landscape: boolean;
  format: string;
  printBackground: boolean;
  scale: number;
  omitBackground: boolean;
}

/** Message sent to the Rust server via stdout. */
type WorkerMessage =
  | { type: "ready"; ready: boolean; error?: string }
  | { type: "response"; id: string; success: boolean; error?: string };

// --- Constants ---

const PAGE_TIMEOUT_MS = 25_000;

// --- Global error handlers ---

process.on("unhandledRejection", (err: unknown) => {
  process.stderr.write(`Unhandled rejection: ${err}\n`);
});

process.on("uncaughtException", (err: Error) => {
  process.stderr.write(`Uncaught exception: ${err}\n`);
  process.exit(1);
});

// --- Helpers ---

/** Returns the Chrome/Chromium binary path, checking `CHROME_EXECUTABLE_PATH` then platform defaults. */
function detectChromePath(): string | null {
  const envPath = process.env.CHROME_EXECUTABLE_PATH;
  if (envPath) return envPath;

  const platform = os.platform();
  const candidates: string[] =
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

/** Safely extracts a message string from an unknown error value. */
function toErrorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

/** Writes a JSON message to stdout (consumed by the Rust server). */
function sendMessage(msg: WorkerMessage): void {
  process.stdout.write(JSON.stringify(msg) + "\n");
}

// --- Main ---

async function main(): Promise<void> {
  const chromePath = detectChromePath();
  if (!chromePath) {
    sendMessage({
      type: "ready",
      ready: false,
      error:
        "Chrome executable not found. Set CHROME_EXECUTABLE_PATH environment variable.",
    });
    process.exit(1);
  }

  let browser: Browser;
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
    sendMessage({
      type: "ready",
      ready: false,
      error: `Failed to launch Chrome: ${toErrorMessage(err)}`,
    });
    process.exit(1);
  }

  // Track in-flight render IDs so we can send error responses on disconnect.
  const inFlightIds = new Set<string>();

  // If Chrome disconnects unexpectedly, notify all in-flight renders and exit.
  browser.on("disconnected", () => {
    process.stderr.write("Chrome disconnected unexpectedly, exiting.\n");
    for (const id of inFlightIds) {
      sendMessage({ type: "response", id, success: false, error: "Chrome disconnected unexpectedly" });
    }
    inFlightIds.clear();
    process.exit(1);
  });

  sendMessage({ type: "ready", ready: true });

  // Concurrency is controlled by the Rust semaphore (max_concurrent_renders).
  // Each job gets its own Chrome page, so they can run in parallel.

  /** Opens a new Chrome page, loads the HTML, generates a PDF, and reports the result. */
  async function renderPdf(req: WorkerRequest): Promise<void> {
    inFlightIds.add(req.id);
    let page: Page | undefined;
    try {
      const htmlContent = await fs.promises.readFile(req.htmlPath, "utf-8");

      page = await browser.newPage();
      page.setDefaultTimeout(PAGE_TIMEOUT_MS);
      page.setDefaultNavigationTimeout(PAGE_TIMEOUT_MS);

      // Block all external network requests to prevent SSRF — only data URIs
      // (inline images/fonts) and about:blank are allowed through.
      await page.setRequestInterception(true);
      page.on("request", (httpReq: HTTPRequest) => {
        const url = httpReq.url();
        if (url.startsWith("data:image/") || url.startsWith("data:font/") || url === "about:blank") {
          httpReq.continue();
        } else {
          httpReq.abort("blockedbyclient");
        }
      });

      await page.setContent(htmlContent, { waitUntil: "load", timeout: PAGE_TIMEOUT_MS });
      await page.pdf({
        path: req.pdfPath,
        format: req.format as PaperFormat,
        landscape: req.landscape,
        printBackground: req.printBackground,
        scale: req.scale,
        omitBackground: req.omitBackground,
        margin: { top: "1cm", right: "1cm", bottom: "1cm", left: "1cm" },
        timeout: PAGE_TIMEOUT_MS,
      });

      sendMessage({ type: "response", id: req.id, success: true });
    } catch (err) {
      sendMessage({ type: "response", id: req.id, success: false, error: toErrorMessage(err) });
    } finally {
      inFlightIds.delete(req.id);
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

  rl.on("line", (line: string) => {
    let request: WorkerRequest;
    try {
      request = JSON.parse(line) as WorkerRequest;
    } catch (err) {
      console.error(`[worker] Failed to parse stdin JSON: ${toErrorMessage(err)}`);
      return;
    }

    if (!request.id || !request.htmlPath || !request.pdfPath) {
      if (request.id) sendMessage({ type: "response", id: request.id, success: false, error: "Missing required fields" });
      return;
    }
    renderPdf(request).catch((err: unknown) => {
      sendMessage({ type: "response", id: request.id, success: false, error: toErrorMessage(err) });
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
