import type { ReactNode } from "react";
import { ModalOverlay, Modal as RACModal, Dialog } from "react-aria-components";

// Modal with focus trap, Esc-to-close, and click-outside dismissal (React Aria).
// Controlled by render: parent renders <Modal> while open and gets onClose on any
// dismissal. `wide` is an optional max-width override for the bundle editor.
export function Modal(
  { onClose, label = "Dialog", wide, children }:
  { onClose: () => void; label?: string; wide?: string; children: ReactNode },
) {
  return (
    <ModalOverlay
      isOpen
      isDismissable
      onOpenChange={(o) => { if (!o) onClose(); }}
      style={{ background: "var(--overlay)" }}
      className="fixed inset-0 z-10 flex items-start justify-center overflow-auto px-4 py-10"
    >
      <RACModal className={`w-full bg-card border border-border rounded-lg ${wide ?? "max-w-[820px]"}`}>
        <Dialog aria-label={label} className="relative outline-none px-7 pt-5 pb-7">
          {({ close }) => (
            <>
              <button
                onClick={close}
                aria-label="close"
                className="absolute top-1.5 right-3 cursor-pointer border-0 bg-transparent p-1 text-2xl leading-none text-muted"
              >
                ×
              </button>
              {children}
            </>
          )}
        </Dialog>
      </RACModal>
    </ModalOverlay>
  );
}
