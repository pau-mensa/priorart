from priorart.excerpt import excerpt


def test_short_text_returned_whole():
    assert excerpt("  hello world  ", ["x"], 400) == "hello world"


def test_window_with_match_near_end_is_selected():
    text = ("filler word " * 100) + "the NCCL barrier timed out on rank three" + (" tail" * 30)
    out = excerpt(text, ["nccl", "barrier"], 80)
    assert "NCCL barrier" in out
    assert out.startswith("…")
    assert len(out) <= 82


def test_no_overlap_falls_back_to_prefix():
    text = "alpha " * 200
    out = excerpt(text, ["zzz"], 60)
    assert out.startswith("alpha")
    assert out.endswith("…")
