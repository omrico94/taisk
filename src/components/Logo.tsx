import styles from "./Logo.module.css";

// taisk mark, "Checkbox Intelligence" design system (see design handoff
// README §1). Geometry: viewBox 0 0 64 64, tile inset 5px on each side (54×54,
// rx 16), tick `M19 34 L28 43 L43 27`, dot `cx=49 cy=19 r=4.6`. At the
// toolbar's 26px size the mark is a solid lime tile with the tick+dot drawn
// in ink (the 24-31px rung of the handoff's size ladder) — this component
// only has one call site (the toolbar), so it doesn't implement the ≥32px
// outline or <24px tick-only rungs.
function Mark() {
  return (
    <svg className={styles.mark} viewBox="0 0 64 64" role="img" aria-label="taisk">
      <rect x={5} y={5} width={54} height={54} rx={16} fill="var(--tk-lime)" />
      <path
        d="M19 34 L28 43 L43 27"
        fill="none"
        stroke="var(--tk-on-lime)"
        strokeWidth={5.6}
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <circle cx={49} cy={19} r={4.6} fill="var(--tk-on-lime)" />
    </svg>
  );
}

// `ta` + the tick-and-dot glyph (standing in for the "i") + `sk`. The glyph
// is inline SVG, not a font character, sized in `em` so it tracks the
// wordmark's own font-size (see design handoff README §1).
function Wordmark() {
  return (
    <span className={styles.wordmark}>
      ta
      <svg viewBox="15 10 40 37" aria-label="i" className={styles.glyph}>
        <path
          d="M19 32 L28 43 L43 24"
          fill="none"
          stroke="var(--tk-lime)"
          strokeWidth={8}
          strokeLinecap="round"
          strokeLinejoin="round"
        />
        <circle cx={49} cy={14} r={6} fill="var(--tk-lime)" />
      </svg>
      sk
    </span>
  );
}

// The <24px rung of the handoff's size ladder: solid lime tile, tick only
// (the dot is dropped at this size). Used by the popup headers. Matches the
// design handoff's popup-header icon exactly: a lime tile (border-radius
// 5/18 of its size) with a small centered check glyph (13/18 of the tile),
// not a scaled-down copy of the full Mark — the check reads bolder and sits
// with more breathing room than naively scaling Mark down would produce.
export function TickMark({ size = 18 }: { size?: number }) {
  const iconSize = (size * 13) / 18;
  return (
    <div
      style={{
        width: size,
        height: size,
        borderRadius: (size * 5) / 18,
        background: "var(--tk-lime)",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        flex: "0 0 auto",
      }}
    >
      <svg width={iconSize} height={iconSize} viewBox="0 0 64 64" role="img" aria-label="taisk">
        <path
          d="M19 35 L28 44 L45 25"
          fill="none"
          stroke="var(--tk-on-lime)"
          strokeWidth={10}
          strokeLinecap="round"
          strokeLinejoin="round"
        />
      </svg>
    </div>
  );
}

interface Props {
  /** Renders the "taisk" wordmark next to the mark. Default true. */
  withWordmark?: boolean;
}

export function Logo({ withWordmark = true }: Props) {
  return (
    <div className={styles.lockup}>
      <Mark />
      {withWordmark && <Wordmark />}
    </div>
  );
}
