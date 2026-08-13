import type { ReactNode } from "react";

export interface ProviderMeta {
  owner: string;
  legalName: string;
}

/** Word-mark owners only. These are not licenses to use official logos. */
export const PROVIDER_META: Record<string, ProviderMeta> = {
  codex: { owner: "OpenAI", legalName: "Codex" },
  copilot: { owner: "GitHub", legalName: "GitHub Copilot" },
  cursor: { owner: "Anysphere", legalName: "Cursor" },
  grok: { owner: "xAI", legalName: "Grok" },
};

export function providerOwner(id: string) {
  return PROVIDER_META[id]?.owner ?? "Third party";
}

const marks: Record<string, ReactNode> = {
  // Generic code brackets — not the OpenAI blossom.
  codex: <path d="M9 8 5 12l4 4M15 8l4 4-4 4" />,
  // Two nodes — not Invertocat or the Copilot product icon.
  copilot: (
    <>
      <circle cx="9" cy="12" r="3.2" />
      <circle cx="15" cy="12" r="3.2" />
    </>
  ),
  // Text caret — not the Cursor cube.
  cursor: <path d="M8 5v14M16 5v14M8 12h8" />,
  // Four-point spark — not the xAI / Grok singularity mark.
  grok: <path d="M12 4v16M4 12h16M7.2 7.2l9.6 9.6M16.8 7.2l-9.6 9.6" />,
};

export function ProviderMark({ id, size = 18, small = false }: { id: string; size?: number; small?: boolean }) {
  return (
    <span className={small ? `provider-logo small provider-${id}` : `provider-logo provider-${id}`} aria-hidden="true">
      <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} strokeLinecap="round" strokeLinejoin="round">
        {marks[id] ?? marks.codex}
      </svg>
    </span>
  );
}

export const TRADEMARK_NOTICE =
  "Codex, GitHub Copilot, Cursor, and Grok name compatible CLIs. Those names and marks belong to OpenAI, GitHub, Microsoft, Anysphere, and xAI. Alfred is independent and is not affiliated with, endorsed by, or sponsored by them.";
