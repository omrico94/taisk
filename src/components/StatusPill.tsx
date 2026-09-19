import styles from "./StatusPill.module.css";

interface Props {
  workingCount: number;
  waitingCount: number;
}

export function StatusPill({ workingCount, waitingCount }: Props) {
  return (
    <div className={styles.pill}>
      <span className={`${styles.dot} ${styles.workingDot}`} />
      <span className={styles.count}>{workingCount}</span>
      <span className={styles.label}>working</span>
      <span className={styles.divider}>·</span>
      <span className={`${styles.dot} ${styles.waitingDot}`} />
      <span className={styles.count}>{waitingCount}</span>
      <span className={styles.label}>need you</span>
    </div>
  );
}
