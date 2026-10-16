import json

from vroom_tui.coverage import load_artifacts, render, summarize


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


def test_load_artifacts_aggregates_functions_and_first_seen_inputs(tmp_path):
    coverage_path = tmp_path / "coverage.json"
    blocks_path = tmp_path / "blocks.json"
    coverage_path.write_text(json.dumps({
        "executions": 17,
        "new_edges": 3,
        "blocks": {
            "1": {"hits": 4, "first_input": "abc123"},
            "2": {"hits": 0, "first_input": None},
        },
    }), encoding="utf-8")
    blocks_path.write_text(json.dumps([
        {"id": 1, "gpa": 4096, "va": 18446603336221204480, "size": 4,
         "module": "HEVD.sys", "function": "Trigger"},
        {"id": 2, "gpa": 4100, "va": 18446603336221204484, "size": 8,
         "module": "HEVD.sys", "function": "Trigger"},
    ]), encoding="utf-8")

    summary, functions, blocks = load_artifacts(coverage_path)

    assert summary == {
        "executions": 17,
        "new_edges": 3,
        "covered_blocks": 1,
        "total_blocks": 2,
    }
    assert [(row.module, row.name, row.covered, row.total, row.hits)
            for row in functions] == [("HEVD.sys", "Trigger", 1, 2, 4)]
    assert [block["first_input"] for block in blocks] == ["abc123", None]
