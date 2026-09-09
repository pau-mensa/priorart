"""FastAPI transport for the protocol. See docs/protocol.md."""

from __future__ import annotations

from typing import Any

from fastapi import FastAPI, Request, status
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse, Response
from pydantic import BaseModel, Field

from . import __version__
from .service import InvalidInput, Service
from .store import RecordDeleted, RecordNotFound, SearchNotFound


class PutRequest(BaseModel):
    text: str
    metadata: dict[str, Any] | None = None
    id: str | None = None


class PutResponse(BaseModel):
    id: str
    revision: int


class RecordResponse(BaseModel):
    id: str
    revision: int
    text: str
    metadata: dict[str, Any] | None
    created_at: str


class SearchRequest(BaseModel):
    text: str
    filters: dict[str, Any] | None = None
    limit: int = Field(default=10)


class HitResponse(BaseModel):
    id: str
    revision: int
    score: float
    score_semantics: str
    excerpt: str
    metadata: dict[str, Any] | None


class SearchResponse(BaseModel):
    search_id: str
    hits: list[HitResponse]
    timings: dict[str, float]
    gatherer: str


class ReportRequest(BaseModel):
    record_id: str
    text: str
    revision: int | None = None
    search_id: str | None = None


class ReportResponse(BaseModel):
    id: str


class ReportOut(BaseModel):
    id: str
    record_id: str
    revision: int | None
    search_id: str | None
    text: str
    created_at: str


class ReportsResponse(BaseModel):
    reports: list[ReportOut]


def _error(status_code: int, code: str, message: str) -> JSONResponse:
    return JSONResponse(
        status_code=status_code, content={"error": {"code": code, "message": message}}
    )


def create_app(service: Service) -> FastAPI:
    app = FastAPI(title="priorart", version=__version__)
    app.state.service = service

    @app.exception_handler(InvalidInput)
    async def _invalid(_: Request, error: InvalidInput) -> JSONResponse:
        return _error(status.HTTP_400_BAD_REQUEST, "invalid_input", str(error))

    @app.exception_handler(RecordNotFound)
    async def _not_found(_: Request, error: RecordNotFound) -> JSONResponse:
        return _error(status.HTTP_404_NOT_FOUND, "record_not_found", f"no record {error}")

    @app.exception_handler(SearchNotFound)
    async def _search_not_found(_: Request, error: SearchNotFound) -> JSONResponse:
        return _error(status.HTTP_404_NOT_FOUND, "search_not_found", f"no search {error}")

    @app.exception_handler(RecordDeleted)
    async def _deleted(_: Request, error: RecordDeleted) -> JSONResponse:
        return _error(status.HTTP_410_GONE, "record_deleted", f"record {error} was deleted")

    @app.exception_handler(RequestValidationError)
    async def _validation(_: Request, error: RequestValidationError) -> JSONResponse:
        first = error.errors()[0] if error.errors() else {}
        location = ".".join(str(part) for part in first.get("loc", ()) if part != "body")
        message = first.get("msg", "invalid request")
        return _error(
            422,
            "validation_error",
            f"{location}: {message}" if location else message,
        )

    @app.get("/healthz")
    def healthz() -> dict[str, Any]:
        return service.health()

    @app.post("/v1/records", status_code=status.HTTP_201_CREATED, response_model=PutResponse)
    def put_record(body: PutRequest) -> PutResponse:
        record_id, revision = service.put(body.text, body.metadata, body.id)
        return PutResponse(id=record_id, revision=revision)

    @app.get("/v1/records/{record_id}", response_model=RecordResponse)
    def get_record(record_id: str, revision: int | None = None) -> RecordResponse:
        item = service.get(record_id, revision)
        return RecordResponse(
            id=item.record_id,
            revision=item.revision,
            text=item.text or "",
            metadata=item.metadata,
            created_at=item.created_at,
        )

    @app.delete("/v1/records/{record_id}", status_code=status.HTTP_204_NO_CONTENT)
    def delete_record(record_id: str) -> Response:
        service.delete(record_id)
        return Response(status_code=status.HTTP_204_NO_CONTENT)

    @app.post("/v1/search", response_model=SearchResponse)
    def search(body: SearchRequest) -> SearchResponse:
        outcome = service.search(body.text, body.filters, body.limit)
        return SearchResponse(
            search_id=outcome.search_id,
            hits=[HitResponse(**hit.__dict__) for hit in outcome.hits],
            timings=outcome.timings,
            gatherer=outcome.gatherer,
        )

    @app.post("/v1/reports", status_code=status.HTTP_201_CREATED, response_model=ReportResponse)
    def report(body: ReportRequest) -> ReportResponse:
        report_id = service.report(body.record_id, body.text, body.revision, body.search_id)
        return ReportResponse(id=report_id)

    @app.get("/v1/records/{record_id}/reports", response_model=ReportsResponse)
    def reports(record_id: str) -> ReportsResponse:
        return ReportsResponse(
            reports=[ReportOut(**item.__dict__) for item in service.reports(record_id)]
        )

    return app
