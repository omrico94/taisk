import styles from "./AskMemoryButton.module.css";

interface Props {
  onClick: () => void;
}

export function AskMemoryButton({ onClick }: Props) {
  return (
    <button className={styles.button} onClick={onClick}>
      <span>✦</span> Ask memory <span className={styles.hint}>⌘K</span>
    </button>
  );
}
