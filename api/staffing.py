"""Versioned Owner staffing guidance; daemons cache it for offline launches.

As with board writes, the authenticated host daemon stamps the actor. Agent
sessions cannot call staffing.set; a shared API token is a trusted-host token.
"""
from datetime import datetime
import unicodedata
from typing import Literal

from fastapi import APIRouter, Depends, HTTPException, Request
from pydantic import BaseModel, ConfigDict, Field, field_validator

from api.auth import verify_token

Engine = Literal["codex", "claude-code"]
WorkKind = Literal["math", "design", "architecture", "metric", "coding", "fanout"]


class Preference(BaseModel):
    model_config = ConfigDict(extra="forbid")
    engine: Engine
    strength: Literal["strong", "lean"]
    reason: str = Field(min_length=1, max_length=500)
    expires_at: datetime

    @field_validator("reason")
    @classmethod
    def one_line(cls, value: str) -> str:
        if not value.strip() or any(unicodedata.category(c) == "Cc" for c in value):
            raise ValueError("reason must be a nonempty single line")
        return value.strip()

    @field_validator("expires_at")
    @classmethod
    def timezone_required(cls, value: datetime) -> datetime:
        if value.tzinfo is None:
            raise ValueError("expires_at must include a timezone")
        return value


class Policy(BaseModel):
    model_config = ConfigDict(extra="forbid")
    default_engine: Engine
    rules: dict[WorkKind, Engine]
    preference: Preference | None = None


class SetPolicy(BaseModel):
    model_config = ConfigDict(extra="forbid")
    expected_version: int = Field(ge=0, le=2**63 - 2)
    policy: Policy
    actor: Literal["owner"]


router = APIRouter(dependencies=[Depends(verify_token)])


@router.get("/staffing")
async def get_policy(request: Request):
    row = await request.app.state.pool.fetchrow(
        "SELECT version, policy, updated_at FROM staffing_policy WHERE org_key='default'")
    if row is None:
        raise HTTPException(503, "Staffing policy has not been initialized")
    return dict(row)


@router.put("/staffing")
async def set_policy(body: SetPolicy, request: Request):
    row = await request.app.state.pool.fetchrow(
        """UPDATE staffing_policy SET policy=$1, version=version+1,
           updated_by='owner', updated_at=now()
           WHERE org_key='default' AND version=$2
           RETURNING version, policy, updated_at""",
        body.policy.model_dump(mode="json"), body.expected_version)
    if row is None:
        raise HTTPException(409, "Staffing policy changed; reload before saving")
    return dict(row)
