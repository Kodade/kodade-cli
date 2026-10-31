import unittest

from terminal_frames import SYNC_BEGIN, SYNC_END, completed_frame_contains


class CompletedFramesTest(unittest.TestCase):
    def test_queued_draws_need_no_quiet_window(self):
        output = SYNC_BEGIN + b"osc8" + SYNC_END + SYNC_BEGIN + b"next draw"
        self.assertTrue(completed_frame_contains(output, b"osc8"))

    def test_fragmented_frame_requires_its_complete_terminator(self):
        output = SYNC_BEGIN + b"PARTIAL FINAL" + SYNC_END
        for length in range(len(output)):
            self.assertFalse(completed_frame_contains(output[:length], b"PARTIAL", b"FINAL"))
        self.assertTrue(completed_frame_contains(output, b"PARTIAL", b"FINAL"))

    def test_content_in_separate_frames_does_not_prove_atomic_presentation(self):
        output = SYNC_BEGIN + b"PARTIAL" + SYNC_END + SYNC_BEGIN + b"FINAL" + SYNC_END
        self.assertFalse(completed_frame_contains(output, b"PARTIAL", b"FINAL"))


if __name__ == "__main__":
    unittest.main()
