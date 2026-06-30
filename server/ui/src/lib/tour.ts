import { driver } from "driver.js";
import "driver.js/dist/driver.css";

// Guided product tour (driver.js — zero runtime deps). First-run autostart is gated on a
// localStorage flag, same pattern as lib/theme.ts and lib/creds.ts; the header "❓ Tour"
// button always restarts it. Steps target elements by `data-tour="…"` attributes (set in
// Dashboard) so they survive Tailwind class churn; steps with no element are centered.
const SEEN_KEY = "mk_tour_seen";

export const tourSeen = (): boolean => !!localStorage.getItem(SEEN_KEY);
export const markTourSeen = (): void => localStorage.setItem(SEEN_KEY, "1");

export function startTour(): void {
  driver({
    showProgress: true,
    nextBtnText: "Next",
    prevBtnText: "Back",
    doneBtnText: "Done",
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
          description: "Each run shows up here with its status. Click one to open it.",
          side: "right",
          align: "start",
        },
      },
      {
        element: '[data-tour="detail"]',
        popover: {
          title: "Logs & summary",
          description:
            "The job view tracks <b>live phases</b> (fetch → build → boot → run), an <b>AI summary</b> " +
            "of what happened and why, and the full <b>logs</b> — build, dmesg, console, exec.",
          side: "left",
          align: "start",
        },
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
        popover: {
          title: "Refine a reproducer",
          description:
            "On any finished job, <b>Refine ✨</b> hands the reproducer and its logs back to the agent " +
            "to fix — and you can add a prompt to steer it. You can also <b>Refine</b> straight from " +
            "the bundle editor to improve a reproducer you're editing.",
        },
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
