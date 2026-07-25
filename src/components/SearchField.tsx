import styles from "./SearchField.module.css";

interface Props {
  value: string;
  onChange: (v: string) => void;
}

export function SearchField({ value, onChange }: Props) {
  return (
    <div className={styles.field}>
      <span className={styles.glyph}>⌕</span>
      <input
        className={styles.input}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder="Filter sessions — or ⌘K to search everything you've ever asked"
      />
    </div>
  );
}
