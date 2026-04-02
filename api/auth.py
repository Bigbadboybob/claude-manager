from fastapi import Header, HTTPException
from dispatch.config import API_TOKEN


async def verify_token(authorization: str = Header()):
    if not API_TOKEN:
        return  # No auth configured (dev mode)
    expected = f"Bearer {API_TOKEN}"
    if authorization != expected:
        raise HTTPException(status_code=401, detail="Invalid token")
