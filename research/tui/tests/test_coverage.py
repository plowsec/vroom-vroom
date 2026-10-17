from vroom_tui.coverage import render, summarize


def test_function_rows_are_sorted_by_coverage_then_name():
    summary = {
        "total_blocks": 5,
        "covered_blocks": 3,
        "executions": 4,
        "new_edges": 2,
        "functions": {"z": [2, 1, 1], "a": [1, 1, 3]},
    }
    rows = summarize(summary)
    assert [row.name for row in rows] == ["a", "z"]
    assert rows[0].percent == 100.0
    assert "blocks 3/5" in render(summary)
    assert "new edges 2" in render(summary)
