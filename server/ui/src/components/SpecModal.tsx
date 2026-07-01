import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import specMd from "../../../../docs/reproducer-spec.md?raw";
import { Modal } from "./ui/Modal";

// Modal overlay rendering the embedded reproducer spec (docs/reproducer-spec.md).
export function SpecModal({ onClose }: { onClose: () => void }) {
  return (
    <Modal onClose={onClose} label="Reproducer spec">
      <div className="mb-2">
        <a className="linkbtn" target="_blank" rel="noreferrer"
          href="https://github.com/elazarl/mackernel/blob/main/docs/reproducer-spec.md">
          View on GitHub ↗
        </a>
      </div>
      <div className="md"><ReactMarkdown remarkPlugins={[remarkGfm]}>{specMd}</ReactMarkdown></div>
    </Modal>
  );
}
