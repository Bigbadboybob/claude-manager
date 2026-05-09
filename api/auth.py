import hmac

from fastapi import Header, HTTPException
from dispatch.config import API_TOKEN


async def verify_token(authorization: str = Header(default="")):
    expected = f"Bearer {API_TOKEN}"
    if not hmac.compare_digest(authorization or "", expected):
        raise HTTPException(status_code=401, detail="Invalid token")
