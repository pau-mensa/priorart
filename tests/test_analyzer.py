from priorart.analyzer import tokens


def test_tokens_casefold_and_strip_accents():
    assert tokens("CUDA_ERROR Illegal-Address café") == [
        "cuda_error",
        "illegal",
        "address",
        "cafe",
    ]


def test_tokens_empty():
    assert tokens("   ") == []
