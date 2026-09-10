# Audyt odziedziczonych błędów grafu i metadanych MCP

Zakres: fork na HEAD przekazanym do audytu (`35a4ee3`), odczyt źródeł i testów. „Naprawione” oznacza zachowanie tego checkoutu, nie naprawę historycznych danych.

| # | Status | Wniosek |
|---|---|---|
| 1 | naprawione | Batch `delete_relations` wykonuje parametryzowany `DELETE` dla każdej relacji. |
| 2 | odziedziczone | `describe_entity` zwraca wyłącznie `Entity`, mimo obietnicy relacji, neighbours i degree. |
| 3 | częściowo naprawione | Nowe relacje są deduplikowane logicznie, ale brak `UNIQUE` i migracji czyszczącej stare duplikaty. |
| 4 | naprawione | MutationService obejmuje merge i zapis outboxa jedną transakcją. |
| 5 | odziedziczone | Brak pochodzenia obserwacji po merge. |
| 6 | zmienione częściowo | `upsert_entities` zmienia typ istniejącej encji, wbrew opisowi; nie ma rename ani jawnego retype. |
| 7 | częściowo naprawione | `created_us` jest zapisywane, ale niewystawione; brak `occurred_at` per obserwacja. |
| 8 | naprawione | Wszystkie eksponowane operacje kasujące mają `destructiveHint: true`. |

## 1. Batch `delete_relations`

**Naprawione.** `MutationRequest::DeleteRelations` iteruje wejście i dla każdej trójki wykonuje stały, parametryzowany `DELETE`; nie buduje listy `SELECT` ([mutation.rs](../../crates/mcpmem-core/src/mutation.rs:655)). Test pokrywa batch z fizycznym duplikatem i sprawdza licznik ([mutation_service.rs](../../tests/mutation_service.rs:362)).

**Test regresji:** E2E MCP: trzy różne relacje w jednym `delete_relations`; `isError == false` i żadnej z nich nie zwraca `open_nodes`.

## 2. `describe_entity`

**Odziedziczone, reprodukcja statyczna.** Manifest obiecuje relacje incydentne, neighbours i degree ([tools.json](../../tools.json:247)), ale implementacja jest wyłącznie `get_entity` ([graph.rs](../../crates/mcpmem-core/src/graph.rs:1559)), a handler serializuje ten `Entity` ([memory.rs](../../src/actions/memory.rs:465)).

**Rekomendacja:** dodać osobny `EntityDescription { entity, relations, neighbors, degree }`; nie zmieniać potajemnie istniejącego `Entity`. Test musi najpierw być czerwony: `A <- B` i `A -> C`, następnie `describe_entity(A)` ma zwrócić dwie relacje z kierunkiem, neighbours `{B,C}` oraz `degree == 2`.

## 3. Duplikaty relacji po merge

**Częściowo naprawione.** `create_relation` używa `INSERT ... WHERE NOT EXISTS` na `(from_id,to_id,type_id)` ([mutation.rs](../../crates/mcpmem-core/src/mutation.rs:563)); merge przekierowuje relacje przez tę funkcję ([mutation.rs](../../crates/mcpmem-core/src/mutation.rs:689)). To eliminuje repro dla nowych danych.

Schemat nie wymusza tej własności: `relation` ma zwykłe indeksy, bez `UNIQUE` ([graph.rs](../../crates/mcpmem-core/src/graph.rs:466)). Historyczne duplikaty przetrwają merge, a inna ścieżka SQL może je wprowadzić.

**Rekomendacja/test:** migracja transakcyjnie deduplikująca po trójce, potem unikalny indeks `(from_id,to_id,type_id)`. Zasiej fizyczny duplikat SQL, uruchom migrację, sprawdź jeden wiersz; następnie merge przy istniejącym `P -> Y` i sprawdź jeden wiersz oraz liczniki.

## 4. Atomowość merge

**Naprawione.** `apply_inner` otwiera `TxGuard` przed snapshotem, wykonuje całą mutację i zapisuje change events/outbox przed commitem ([mutation.rs](../../crates/mcpmem-core/src/mutation.rs:231)). Przenoszenie obserwacji, przekierowanie relacji i usunięcie source są jedną gałęzią `execute` ([mutation.rs](../../crates/mcpmem-core/src/mutation.rs:689)).

**Test regresji:** trigger w testowej SQLite abortuje `DELETE FROM entity` dla source; po błędzie source, target, obserwacje i relacje są identyczne jak przed merge.

## 5. Pochodzenie obserwacji

**Odziedziczone.** `observation` przechowuje tylko `entity_id`, `idx`, `body`, `created_us` ([graph.rs](../../crates/mcpmem-core/src/graph.rs:455)); merge kopiuje same teksty do target ([mutation.rs](../../crates/mcpmem-core/src/mutation.rs:693)). `change_event` ma payload zmiany, nie `source_entity` per obserwacja ([0001_change_events.sql](../../crates/mcpmem-core/migrations/0001_change_events.sql:6)).

**Rekomendacja/test:** wymagająca decyzji migracja: `source_entity_id` albo immutable observation IDs z tabelą provenance, nie string w `body`. Po decyzji merge i drugi merge muszą zachować original source każdej obserwacji.

## 6. Typ i nazwa encji

**Zmienione częściowo.** `upsert_entities` aktualizuje `type_id` istniejącej encji ([mutation.rs](../../crates/mcpmem-core/src/mutation.rs:615)); jest na to test ([graph.rs](../../crates/mcpmem-core/src/graph.rs:2388)). Opis nadal mówi, że typ jest stosowany tylko przy tworzeniu ([tools.json](../../tools.json:277)). Nie ma publicznego rename ani jawnego retype.

**Rekomendacja/test:** najpierw zdecydować kompatybilność `upsert`; następnie jawne `retype_entity` i `rename_entity` z błędem kolizji albo wersjonowana korekta manifestu. E2E ma utrwalić wybrane zachowanie upsert; rename ma zachować obie strony relacji i odrzucić kolizję.

## 7. Czas obserwacji

**Częściowo naprawione.** `created_us` istnieje w schemacie i jest ustawiane przez serwer ([graph.rs](../../crates/mcpmem-core/src/graph.rs:455), [mutation.rs](../../crates/mcpmem-core/src/mutation.rs:516)). Nie jest częścią odpowiedzi `Entity`, nie ma `occurred_at`, a `change_event.occurred_at_us` dotyczy mutacji, nie faktu ([0001_change_events.sql](../../crates/mcpmem-core/migrations/0001_change_events.sql:6)).

**Rekomendacja/test:** wersjonowany obiekt obserwacji: `body`, server-owned `createdAt`, opcjonalne `occurredAt`, z migracją `NULL occurredAt`. Serwer ignoruje klientowski `createdAt`, zachowuje `occurredAt`, eksport sortuje stabilnie po `createdAt,id`.

## 8. `destructiveHint` i `readOnlyHint`

**Naprawione dla obecnego MCP.** `delete_entities`, `delete_observations`, `delete_relations` mają `readOnlyHint:false, destructiveHint:true` ([tools.json](../../tools.json:71)), tak samo `merge_entities` ([tools.json](../../tools.json:311)) i `vector_delete_embedding` ([vector_tools.json](../../vector_tools.json:53)). Pozostałe mutatory są non-read-only, lecz non-destructive; czytelniki mają `readOnlyHint:true`.

Metadane są ładowane z JSON wbudowanego przez `include_str!` i zwracane w kolejności plików ([server.rs](../../src/server.rs:563), [server.rs](../../src/server.rs:595)). Zbiór zależy celowo od kategorii/feature'ów. Zastrzeżenie: flagi kategorii są globalnymi atomikami ustawianymi w konstruktorze ([server.rs](../../src/server.rs:244)), więc dwa serwery o różnych konfiguracjach w jednym procesie nie izolują `tools/list`; CLI z jednym serwerem nie jest tym dotknięte.

**Test regresji:** E2E `tools/list`, parse JSON i porównanie mapy nazwa → adnotacje z jawną tabelą polityki. Każdy reader ma `readOnlyHint=true`; `delete_*` i `merge_entities` są destructive; po włączeniu vectors również `vector_delete_embedding`. Po usunięciu globali testować dwie instancje o różnych konfiguracjach w jednym procesie.

## Kolejność pracy

1. Naprawić #2 z czerwonym testem kontraktu — dziś cicho gubi dane.
2. Zbadać dane historyczne, potem #3: migracja czyszcząca i `UNIQUE`.
3. Podjąć wspólną decyzję dla #5–#7; to migracje i publiczne odpowiedzi.
4. Dodać tabelaryczny test #8; przed hostowaniem wieloinstancyjnym usunąć globalne flagi z metadanych.
