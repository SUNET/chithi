# ADR 0046: Microsoft 365 Calendar Room Lookup

## Status
Accepted

## Context
Chithi's Microsoft 365 calendar event form needs to offer available rooms as selectable location suggestions while still allowing free-text locations. Outlook on the web can show room suggestions for work/school tenants, but the Microsoft Graph endpoint behavior varies by tenant configuration.

The simple Outlook endpoint `GET /me/findRooms` can return an empty list even when Outlook web shows rooms. Some tenants expose rooms through room lists (`/me/findRoomLists` followed by `/me/findRooms(RoomList='...')`), while others expose them through Graph Places (`/places/microsoft.graph.room`). Work/school tenants may also require admin consent for place/resource directory reads.

## Decision
Use a layered room lookup strategy in `GraphClient::list_rooms()`:

1. Call `GET /me/findRoomLists`.
2. For each room list, call `GET /me/findRooms(RoomList='<room-list-address>')`.
3. If room lists fail or return no rooms, call `GET /places/microsoft.graph.room?$top=200&$select=displayName,emailAddress`.
4. If Places fails or returns no rooms, fall back to `GET /me/findRooms`.

The backend returns a normalized `{ name, address }` list to the frontend. The frontend shows these rooms as a dropdown under the Location field, and selecting a room fills the location value. The field remains a normal text input, so users can type arbitrary locations when Graph returns no rooms or when they want a non-room location.

## Permissions
The Microsoft OAuth configuration includes the delegated Graph scope `Place.Read.All` so Chithi can read room/place resources for work/school accounts.

This scope is included in both:

- The initial Microsoft OAuth consent scope list.
- `MICROSOFT_GRAPH_SCOPES`, used by `get_graph_token()` when refreshing Graph-scoped tokens.

Work/school tenants may require administrator consent for `Place.Read.All`. Existing accounts may need to sign in again before the new scope is granted.

## Consequences
- O365 room suggestions work across more tenant configurations than `/me/findRooms` alone.
- The UI remains usable when room lookup fails because the Location field accepts free text.
- Adding `Place.Read.All` expands requested Graph permissions; admins should be told this is for calendar room/resource lookup only.
- Debug logs include room endpoint attempts and normalized room names/addresses to help diagnose tenant-specific behavior.
