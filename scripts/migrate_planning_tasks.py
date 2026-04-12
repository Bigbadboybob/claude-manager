#!/usr/bin/env python3
"""One-shot migration: import local .md planning tasks into the database.

Reads from ~/.cm/projects/*/tasks/*.md, parses YAML frontmatter + body,
and inserts into the tasks table with the new planning fields.

Usage:
    python -m scripts.migrate_planning_tasks [--dry-run]
"""

import asyncio
import re
import sys
from pathlib import Path

import asyncpg
import yaml

from dispatch.config import DB_DSN, REPOS

PROJECTS_DIR = Path.home() / ".cm" / "projects"


def slugify(text: str) -> str:
    slug = re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")
    return slug[:50]


def parse_task_file(path: Path) -> dict | None:
    """Parse a planning task .md file into a dict."""
    content = path.read_text()
    trimmed = content.strip()
    if not trimmed.startswith("---"):
        return None

    after_first = trimmed[3:]
    end_idx = after_first.find("\n---")
    if end_idx == -1:
        return None

    yaml_str = after_first[:end_idx]
    body = after_first[end_idx + 4:].strip()

    try:
        front = yaml.safe_load(yaml_str) or {}
    except yaml.YAMLError:
        return None

    # Extract ## Prompt section from body
    prompt = None
    description_lines = []
    in_prompt = False
    for line in body.split("\n"):
        if line.startswith("## Prompt"):
            in_prompt = True
            continue
        if in_prompt:
            if line.startswith("## "):
                in_prompt = False
                description_lines.append(line)
            else:
                if prompt is None:
                    prompt = ""
                prompt = (prompt + "\n" + line) if prompt else line
        else:
            description_lines.append(line)

    description = "\n".join(description_lines).strip()
    if prompt:
        prompt = prompt.strip()

    return {
        "title": front.get("title", path.stem.replace("-", " ")),
        "status": front.get("status", "draft"),
        "difficulty": front.get("difficulty"),
        "depends": front.get("depends", []) or [],
        "branch": front.get("branch"),
        "created": front.get("created"),
        "description": description,
        "prompt": prompt or "",
        "slug": path.stem,
    }


def repo_url_for_project(project_name: str) -> str:
    """Look up the repo URL for a project name."""
    if project_name in REPOS:
        return REPOS[project_name]
    # Fallback: construct a plausible URL
    return f"https://github.com/Bigbadboybob/{project_name}.git"


async def migrate(dry_run: bool = False):
    if not PROJECTS_DIR.is_dir():
        print(f"No projects directory at {PROJECTS_DIR}")
        return

    pool = await asyncpg.create_pool(DB_DSN, min_size=1, max_size=3)

    # Run migrations first
    sql_dir = Path(__file__).parent.parent / "sql"
    async with pool.acquire() as conn:
        for sql_file in sorted(sql_dir.glob("*.sql")):
            await conn.execute(sql_file.read_text())

    imported = 0
    skipped = 0

    for project_dir in sorted(PROJECTS_DIR.iterdir()):
        if not project_dir.is_dir():
            continue
        tasks_dir = project_dir / "tasks"
        if not tasks_dir.is_dir():
            continue

        project_name = project_dir.name
        repo_url = repo_url_for_project(project_name)

        for md_file in sorted(tasks_dir.glob("*.md")):
            task = parse_task_file(md_file)
            if not task:
                print(f"  SKIP (parse error): {md_file}")
                skipped += 1
                continue

            # Check if already exists
            async with pool.acquire() as conn:
                existing = await conn.fetchrow(
                    "SELECT id FROM tasks WHERE project = $1 AND slug = $2",
                    project_name, task["slug"],
                )
                if existing:
                    print(f"  SKIP (exists): {project_name}/{task['slug']}")
                    skipped += 1
                    continue

            # Map status
            status = task["status"]
            if status not in ("draft", "backlog", "in_progress", "done"):
                status = "draft"
            # in_progress in planning means it has an active session, map to backlog
            # since we're not carrying over sessions
            if status == "in_progress":
                status = "backlog"

            if dry_run:
                print(f"  DRY RUN: {project_name}/{task['slug']} ({status})")
                imported += 1
                continue

            async with pool.acquire() as conn:
                await conn.execute(
                    """INSERT INTO tasks (repo_url, repo_branch, name, prompt, status,
                                          priority, project, slug, description, difficulty,
                                          depends, source, is_cloud)
                       VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)""",
                    repo_url,
                    task.get("branch") or "main",
                    task["title"],
                    task["prompt"],
                    status,
                    0,  # priority
                    project_name,
                    task["slug"],
                    task["description"],
                    task["difficulty"],
                    task["depends"],
                    "user",
                    False,  # is_cloud
                )
                print(f"  IMPORTED: {project_name}/{task['slug']} ({status})")
                imported += 1

    await pool.close()
    print(f"\nDone. Imported: {imported}, Skipped: {skipped}")


if __name__ == "__main__":
    dry_run = "--dry-run" in sys.argv
    if dry_run:
        print("=== DRY RUN ===\n")
    asyncio.run(migrate(dry_run=dry_run))
