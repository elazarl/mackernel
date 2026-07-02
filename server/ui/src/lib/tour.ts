import { driver } from "driver.js";
import "driver.js/dist/driver.css";

// Guided product tour (driver.js — zero runtime deps). First-run autostart is gated on a
// localStorage flag, same pattern as lib/theme.ts and lib/creds.ts; the header "❓ Tour"
// button always restarts it. Steps target elements by `data-tour="…"` attributes (set in
// Dashboard / JobDetail) so they survive Tailwind class churn; steps with no element are
// centered. `opts.selectJob` lets the tour open a real job (#1) so the logs/summary steps
// have live content to point at.
const SEEN_KEY = "mk_tour_seen";

export const tourSeen = (): boolean => !!localStorage.getItem(SEEN_KEY);
export const markTourSeen = (): void => localStorage.setItem(SEEN_KEY, "1");

export function startTour(opts: { selectJob?: (id: number) => void } = {}): void {
  const openDemoJob = () => opts.selectJob?.(1); // the tour walks through job #1's logs

  driver({
    showProgress: true,
    nextBtnText: "Next",
    prevBtnText: "Back",
    doneBtnText: "Done",
    // driver.js has no text for the footer "close" button, so inject our own "Skip tour"
    // link (left side of the footer) on every step; it ends the tour immediately.
    onPopoverRender: (popover, { driver: d }) => {
      if (popover.footer.querySelector(".mk-skip")) return;
      const skip = document.createElement("button");
      skip.className = "mk-skip";
      skip.textContent = "Skip tour";
      skip.style.cssText =
        "background:none;border:none;color:#8b949e;cursor:pointer;font-size:13px;" +
        "text-decoration:underline;padding:0;margin-right:auto;";
      skip.addEventListener("click", () => d.destroy());
      popover.footer.insertBefore(skip, popover.footer.firstChild);
    },
    steps: [
      {
        popover: {
          title: "Welcome to the Kernel Reproducer Runner 🐧",
          description:
            "This service <b>compiles real Linux kernels and recreates bugs on them</b> — booting " +
            "every build in an isolated VM and running a reproducer against it, both without and " +
            "with a fix so you can see the difference. Here's the quick tour.",
        },
      },
      {
        element: '[data-tour="examples"]',
        popover: {
          title: "Start from an example",
          description:
            "New here? Click an <b>example</b> to load a ready-made reproducer bundle into the editor.",
          side: "top",
          align: "start",
        },
      },
      {
        element: '[data-tour="submit"]',
        popover: {
          title: "Run it",
          description:
            "Paste your own bundle or pick an example — the editor opens, then hit " +
            "<b>Run reproducer</b>. The runner fetches the source, builds the kernel, boots a VM, " +
            "and runs your reproducer.",
          side: "right",
          align: "start",
        },
      },
      {
        element: '[data-tour="jobs"]',
        popover: {
          title: "Every run is a job",
          description:
            "Each run shows up here with its status. Let's open one — <b>#1</b> — and see what it tells you.",
          side: "right",
          align: "start",
        },
        // Open job #1 so the next two steps have a real job to point at.
        onHighlighted: openDemoJob,
      },
      {
        element: '[data-tour="jobsummary"]',
        popover: {
          title: "Live phases & summary",
          description:
            "Up top, the <b>phases</b> (fetch → build → boot → run) light up as the job progresses, " +
            "and an <b>AI summary</b> explains what the reproducer did and why it passed or failed.",
          side: "left",
          align: "start",
        },
        onHighlightStarted: openDemoJob, // idempotent — keeps the job open if you jump here
      },
      {
        element: '[data-tour="joblogs"]',
        popover: {
          title: "The logs — what they mean",
          description:
            "Every stage is captured here (click a tab):<br>" +
            "• <b>fetch</b> — cloning the kernel source &amp; applying patches<br>" +
            "• <b>compile</b> — the kernel build output (warnings/errors)<br>" +
            "• <b>console</b> — the raw QEMU serial console (the full boot)<br>" +
            "• <b>dmesg</b> — the guest kernel ring buffer (oops, panics, printk)<br>" +
            "• <b>exec</b> — your reproducer's own output inside the guest<br>" +
            "• <b>run</b> — the orchestrator log, with the top-level pass/fail reason",
          side: "left",
          align: "start",
        },
        onHighlightStarted: openDemoJob,
      },
      {
        element: '[data-tour="lkml"]',
        popover: {
          title: "Scaffold from an LKML patch",
          description:
            "<b>Browse LKML</b> to pick a patch series, then <b>Scaffold ✨</b>. An AI agent reads the " +
            "patch and the kernel source, writes a reproducer for the bug it fixes, and runs it.",
          side: "right",
          align: "start",
        },
      },
      {
        element: '[data-tour="refine"]',
        popover: {
          title: "Refine a reproducer",
          description:
            "On any finished job, <b>Refine ✨</b> hands the reproducer and its logs back to the agent " +
            "to fix — and you can add a prompt to steer it. You can also <b>Refine</b> straight from " +
            "the bundle editor to improve a reproducer you're editing.",
          side: "left",
          align: "start",
        },
        onHighlightStarted: openDemoJob, // job #1 must be open for the Refine button to exist
      },
      {
        element: '[data-tour="runlocal"]',
        popover: {
          title: "Run it on your own machine",
          description:
            "Want to reproduce this yourself? <b>Run locally</b> gives you a copy-paste command — " +
            "clone the mackernel repo and point <code>run-kernel.py</code> at this job's bundle to " +
            "build the kernel and boot the reproducer on your own box.",
          side: "left",
          align: "start",
        },
        onHighlightStarted: openDemoJob, // job #1 must be open for the Run locally button to exist
      },
      {
        element: '[data-tour="spec"]',
        popover: {
          title: "The bundle spec",
          description: "The full reproducer-bundle format lives here — open <b>Spec</b> anytime.",
          side: "bottom",
          align: "start",
        },
      },
    ],
  }).drive();
}
