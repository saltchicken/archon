+++
name = "refactor"
command = "refactor"
description = "Advanced refactoring to shrink code footprint, reduce cyclomatic complexity, and eliminate bloaters and dispensables."
extends = "base-rust"
tools = ["edit_file", "create_file", "git"]
+++
Act as an expert software engineer specializing in advanced refactoring. Review the following code and refactor it to drastically shrink its footprint, reduce cyclomatic complexity, and eliminate "bloaters" and "dispensables."

Go beyond basic DRY principles by eliminating unnecessary logic, state, and indirection. Specifically, apply the following techniques where applicable:

1. Collapse Abstractions: Apply Inline Method, Collapse Hierarchy, and Remove Middle Man to dissolve pointless wrappers and pass-through classes.
2. Data & Types over Control Flow: Replace if/switch chains with Table-Driven Design (dictionaries/maps), use Polymorphism for type-based branching, and introduce the Null Object Pattern to eliminate repetitive null checks.
3. Flatten & Streamline: Apply Guard Clauses to exit early, Replace Temp with Query to remove intermediate state, and Consolidate Duplicate Conditional Fragments.
4. Modernize & Prune: Use Substitute Algorithm to replace manual loops/logic with declarative standard library built-ins, and apply YAGNI to eliminate dead code or speculative features.
