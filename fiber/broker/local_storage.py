import json
import sqlite3
import threading
import time

from loguru import logger

from fiber.common.consts import PATH_FIBER_FILE


class LocalStorage:
    def __init__(self, filename: str, store_max_days: int = 90) -> None:
        db_path = PATH_FIBER_FILE + filename
        self._store_max_seconds = 60 * 60 * 24 * store_max_days

        self._connection = sqlite3.connect(db_path, check_same_thread=False)
        self._lock = threading.RLock()

        self._connection.execute(
            """
            CREATE TABLE IF NOT EXISTS records(
                record_id   INTEGER     PRIMARY KEY     AUTOINCREMENT,
                timestamp   INTEGER                     NOT NULL,
                record      JSON                        NOT NULL
            );
        """
        )
        self._connection.execute(
            """
            CREATE UNIQUE INDEX IF NOT EXISTS index_timestamp
                ON records(timestamp);
        """
        )
        logger.debug(f"Connected to {db_path} and created table if not exists")

    def __del__(self) -> None:
        self._connection.close()

    def add_report(self, timestamp: int, report: dict) -> None:
        last_valid_ts = int(time.time()) - self._store_max_seconds

        with self._lock:
            self._delete_old_records(last_valid_ts)
            self._insert_new_records(timestamp, report)

        logger.debug(f"Added report with timestamp {timestamp}")

    def _delete_old_records(self, last_valid_ts: int) -> None:
        with self._connection:
            cursor = self._connection.cursor()
            try:
                cursor.execute("DELETE FROM records WHERE timestamp < ?;", (last_valid_ts,))
                self._connection.commit()
                logger.debug(f"Removed old records older than {last_valid_ts}")
            except sqlite3.OperationalError as exc:
                logger.error(f"OperationalError: {exc}. Failed to delete old records older than {last_valid_ts}")

    def _insert_new_records(self, timestamp: int, report: dict) -> None:
        with self._connection:
            cursor = self._connection.cursor()
            try:
                cursor.execute('INSERT INTO records (timestamp, record) values (?, ?);',
                            [timestamp, json.dumps(report)])

                self._connection.commit()
                logger.debug(
                    f"Added {len(report)} sensor record(s) to database with timestamp {timestamp}"
                )
            except sqlite3.IntegrityError as exc:
                logger.error(f"IntegrityError: {exc}. Could not insert record with timestamp {timestamp}")

