#!/usr/bin/env python3
"""Check four LoRA aggregation identities with exact rational arithmetic.

This is an algebra-only probe, not a trainer or a production aggregation API.
No models, networks or third-party packages are used. Fractions deliberately
avoid interpreting floating-point roundoff as evidence about the identities.
"""

import argparse
from dataclasses import dataclass
from fractions import Fraction
import hashlib
import json
from pathlib import Path


BASELINE_SHA = "f3fa6f4d8bf22cfd2baf490d9c3a08bd39af4d42"
Matrix = list[list[Fraction]]


def matrix(rows) -> Matrix:
    result = [[Fraction(value) for value in row] for row in rows]
    shape(result)
    return result


def shape(value: Matrix) -> tuple[int, int]:
    if not value or not value[0] or any(len(row) != len(value[0]) for row in value):
        raise ValueError("Expected a nonempty rectangular matrix")
    return len(value), len(value[0])


def multiply(left: Matrix, right: Matrix) -> Matrix:
    rows, inner = shape(left)
    right_rows, columns = shape(right)
    if inner != right_rows:
        raise ValueError("Incompatible matrix product dimensions")
    result = [[Fraction(0) for _ in range(columns)] for _ in range(rows)]
    for row in range(rows):
        for index in range(inner):
            for column in range(columns):
                result[row][column] += left[row][index] * right[index][column]
    return result


@dataclass(frozen=True)
class Client:
    name: str
    weight: Fraction
    scaling: Fraction
    a: Matrix
    b: Matrix

    def __post_init__(self):
        if self.weight <= 0 or self.scaling <= 0:
            raise ValueError("These fixtures require positive weights and scaling")
        if shape(self.a)[0] != shape(self.b)[1]:
            raise ValueError("A and B must have the same adapter rank")

    def record(self):
        return {
            "name": self.name,
            "weight_p": self.weight,
            "scaling_s": self.scaling,
            "rank": shape(self.a)[0],
            "A": self.a,
            "B": self.b,
            "unscaled_BA": multiply(self.b, self.a),
        }


def direct_weighted_product(clients: list[Client]) -> Matrix:
    """Independent oracle calculation: sum each client's scalar products.

    This does not call multiply(), stack() or factor_average(). The manually
    calculated fixture matrix supplies a further independent comparison.
    """
    if not clients or sum(c.weight for c in clients) != 1:
        raise ValueError("Client weights must sum exactly to one")
    rows = shape(clients[0].b)[0]
    columns = shape(clients[0].a)[1]
    if any(shape(c.b)[0] != rows or shape(c.a)[1] != columns for c in clients):
        raise ValueError("Client updates must share output and input dimensions")
    return [
        [
            sum(
                c.weight * c.scaling
                * sum(c.b[row][k] * c.a[k][column] for k in range(len(c.a)))
                for c in clients
            )
            for column in range(columns)
        ]
        for row in range(rows)
    ]


def stack(clients: list[Client]) -> tuple[Matrix, Matrix]:
    # B_stack = [p_1 s_1 B_1 | ... | p_n s_n B_n]; A_stack = [A_1; ...; A_n].
    # Keeping all scaling in B avoids square roots of rational weights.
    a = [list(row) for c in clients for row in c.a]
    b = [
        [c.weight * c.scaling * value for c in clients for value in c.b[row]]
        for row in range(len(clients[0].b))
    ]
    return a, b


def factor_average(clients: list[Client]) -> tuple[Matrix, Matrix] | None:
    """Average A and scaling-adjusted B separately, only when ranks agree."""
    if len({len(c.a) for c in clients}) != 1:
        return None
    rank, columns = shape(clients[0].a)
    rows = len(clients[0].b)
    a = [
        [sum(c.weight * c.a[k][j] for c in clients) for j in range(columns)]
        for k in range(rank)
    ]
    b = [
        [sum(c.weight * c.scaling * c.b[i][k] for c in clients) for k in range(rank)]
        for i in range(rows)
    ]
    return a, b


def error(actual: Matrix, expected: Matrix):
    if shape(actual) != shape(expected):
        raise ValueError("Cannot compare matrices with different dimensions")
    difference = [
        [value - expected[i][j] for j, value in enumerate(row)]
        for i, row in enumerate(actual)
    ]
    return {
        "actual_minus_expected": difference,
        "max_absolute_entry": max(abs(v) for row in difference for v in row),
    }


def client(name, weight, scaling, a, b):
    return Client(name, Fraction(weight), Fraction(scaling), matrix(a), matrix(b))


def fixtures():
    return [
        {
            "id": "same_rank_cross_terms",
            "purpose": "Separate factor averaging creates cross terms even at equal rank.",
            "clients": [
                client("one", "1/2", 1, [[1, 0]], [[1], [0]]),
                client("two", "1/2", 1, [[0, 1]], [[0], [1]]),
            ],
            "expected": matrix([["1/2", 0], [0, "1/2"]]),
            "factor_average_should_match": False,
        },
        {
            "id": "gauge_cancellation",
            "purpose": "Equivalent BA factors (A,B) and (-A,-B) average to zero factors.",
            "clients": [
                client("one", "1/2", 1, [[1, 2]], [[3], [4]]),
                client("two", "1/2", 1, [[-1, -2]], [[-3], [-4]]),
            ],
            "expected": matrix([[3, 6], [4, 8]]),
            "factor_average_should_match": False,
        },
        {
            "id": "heterogeneous_ranks_weights_and_scaling",
            "purpose": "Exact stacking supports ranks 1 and 2 with unequal weights and scaling.",
            "clients": [
                client("one", "1/3", 2, [[1, 0, 2]], [[1], [2]]),
                client("two", "2/3", "1/2", [[1, 2, 0], [0, 1, 1]], [[0, 1], [1, -1]]),
            ],
            "expected": matrix([["2/3", "1/3", "5/3"], ["5/3", "1/3", "7/3"]]),
            "factor_average_should_match": None,
        },
        {
            "id": "shared_frozen_a_with_scaling_in_b",
            "purpose": "Factor averaging is exact when A is shared/frozen and scaling is absorbed in B.",
            "clients": [
                client("one", "1/4", 2, [[1, 0, 2], [0, 1, -1]], [[1, 2], [3, 4]]),
                client("two", "3/4", "1/3", [[1, 0, 2], [0, 1, -1]], [[2, -1], [-2, 1]]),
            ],
            "expected": matrix([[1, "3/4", "5/4"], [1, "9/4", "-1/4"]]),
            "factor_average_should_match": True,
        },
    ]


def run_case(fixture):
    clients = fixture["clients"]
    direct = direct_weighted_product(clients)
    stack_a, stack_b = stack(clients)
    stacked = multiply(stack_b, stack_a)
    checks = {
        "direct_matches_manual_fixture": direct == fixture["expected"],
        "stacking_matches_direct": stacked == direct,
    }
    average = factor_average(clients)
    if average is None:
        average_report = {
            "applicable": False,
            "reason": "Ranks differ; elementwise averaging has no shape-compatible definition here.",
        }
        checks["factor_average_inapplicability_expected"] = fixture["factor_average_should_match"] is None
    else:
        average_a, average_b = average
        averaged = multiply(average_b, average_a)
        average_report = {
            "applicable": True,
            "A_bar": average_a,
            "scaled_B_bar": average_b,
            "product": averaged,
            "error_against_direct": error(averaged, direct),
            "matches_direct": averaged == direct,
            "expected_to_match": fixture["factor_average_should_match"],
        }
        checks["factor_average_behavior_matches_expectation"] = (
            (averaged == direct) == fixture["factor_average_should_match"]
        )
        if fixture["id"] == "gauge_cancellation":
            checks["client_products_equal"] = multiply(clients[0].b, clients[0].a) == multiply(clients[1].b, clients[1].a)
            checks["factor_average_is_zero"] = all(v == 0 for row in averaged for v in row)
            checks["expected_update_is_nonzero"] = any(v != 0 for row in direct for v in row)
        if fixture["id"] == "shared_frozen_a_with_scaling_in_b":
            checks["a_is_identical_across_clients"] = all(c.a == clients[0].a for c in clients)
    return {
        "id": fixture["id"],
        "purpose": fixture["purpose"],
        "clients": [c.record() for c in clients],
        "manual_expected": fixture["expected"],
        "direct_weighted_product": direct,
        "direct_error_against_manual": error(direct, fixture["expected"]),
        "stacking": {
            "A_stack": stack_a,
            "B_stack": stack_b,
            "stacked_adapter_width": len(stack_a),
            "product": stacked,
            "error_against_direct": error(stacked, direct),
        },
        "separate_factor_average": average_report,
        "checks": checks,
        "passed": all(checks.values()),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, help="Write JSON to this path instead of stdout")
    args = parser.parse_args()
    cases = [run_case(fixture) for fixture in fixtures()]
    report = {
        "evidence_kind": "algebra_only",
        "baseline_repository_sha": BASELINE_SHA,
        "source": {
            "path": "scripts/probe_federated_lora.py",
            "sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        },
        "arithmetic": "fractions.Fraction; all scalar values serialized as exact rational strings",
        "target": "sum_i p_i * s_i * B_i * A_i",
        "stacking_definition": "B_stack = horizontal_concat(p_i * s_i * B_i); A_stack = vertical_concat(A_i)",
        "factor_average_definition": "(sum_i p_i * s_i * B_i) * (sum_i p_i * A_i)",
        "scope": {
            "third_party_dependencies": [],
            "network_calls": False,
            "model_calls": False,
            "not_evaluated": [
                "training or task quality", "floating-point or quantized implementations",
                "rank compression", "performance or communication costs",
                "distributed execution or recovery", "privacy or security",
                "Shenko runtime integration",
            ],
        },
        "cases": cases,
        "case_count": len(cases),
        "all_passed": all(case["passed"] for case in cases),
    }
    payload = json.dumps(report, indent=2, ensure_ascii=False, default=str) + "\n"
    if args.output is None:
        print(payload, end="")
    else:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(payload, encoding="utf-8")
    return 0 if report["all_passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
