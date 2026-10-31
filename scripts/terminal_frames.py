"""Observe complete synchronized frames without assumptions about PTY scheduling."""

SYNC_BEGIN = b"\x1b[?2026h"
SYNC_END = b"\x1b[?2026l"


def completed_frame_contains(output, *markers):
    """Require every marker in one complete frame, even with queued later draws."""
    offset = 0
    while True:
        begin = output.find(SYNC_BEGIN, offset)
        if begin < 0:
            return False
        end = output.find(SYNC_END, begin + len(SYNC_BEGIN))
        if end < 0:
            return False
        frame = output[begin + len(SYNC_BEGIN):end]
        if all(marker in frame for marker in markers):
            return True
        offset = end + len(SYNC_END)
