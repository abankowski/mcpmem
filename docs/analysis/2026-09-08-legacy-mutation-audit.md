# Audyt odziedziczonych błędów mutacji (1, 3, 4, 5)

**Stan sprawdzony:** `35a4ee3` (2026-09-08). Audyt jest odczytowy względem repozytorium; repro uruchomiono wyłącznie na nowych bazach w `/tmp`.

| Nr | Werdykt | Krótko |
|---|---|---|
| 1 | **Naprawione** | Batch `delete_relations` nie buduje już jednego wadliwego SQL-a. |
| 3 | **Naprawione dla nowych zapisów** | Merge używa idempotentnego inserta relacji i nie tworzy duplikatu. Stare fizyczne duplikaty nadal są możliwe w schemacie. |
| 4 | **Naprawione** | Pełny merge, liczniki i outbox są w jednym `BEGIN IMMEDIATE` / `COMMIT`. |
| 5 | **Odziedziczone** | Obserwacja ma czas utworzenia, ale nie ma pochodzenia encji; merge bezpowrotnie je zaciera. |

## 1. Batch `delete_relations`

### Dowód i repro

`MutationRequest::DeleteRelations` iteruje po relacjach i dla każdej wykonuje pojedynczy, stały statement z pięcioma parametrami [`crates/memory-core/src/mutation.rs:655-660`](../../crates/memory-core/src/mutation.rs#L655-L660). Nie ma więc buildera `SELECT ? ... , SELECT ? ...`, który był przyczyną upstreamowego błędu składni. Wywołanie MCP z dwiema relacjami (`A→B:pracuje`, `C→D:dotyczy`) zwróciło `Relations deleted successfully`; natychmiastowy `graph_stats` zwrócił `{"entities":4,"relations":0}`.

Obecny test E2E obejmuje tylko jedną relację [`tests/e2e.rs:252-260`](../../tests/e2e.rs#L252-L260), więc nie jest testem regresji na odkrytą klasę błędu.

### Rekomendacja

Nie zastępować obecnej pętli wielowierszowym `VALUES`: pętla jest prosta, parametryzowana i cała pętla leży w transakcji usługi mutacji. Dodać test MCP dla **1, 2 i co najmniej 3** elementów (z asercją, że po wywołaniu nie ma żadnej relacji). To zamyka brak testowy bez zmiany schematu i bez migracji.

## 3. Duplikaty relacji po `merge_entities`

### Dowód i repro

Merge przepisuje końce relacji na target, a następnie zawsze przechodzi przez `create_relation` [`mutation.rs:689-705`](../../crates/memory-core/src/mutation.rs#L689-L705). Ten inserter stosuje atomowy warunek `WHERE NOT EXISTS` dla dokładnego `(from_id, to_id, type_id)` [`mutation.rs:563-572`](../../crates/memory-core/src/mutation.rs#L563-L572).

Repro z `P→Y:pracuje` i `P→X:pracuje`, potem `merge(X,Y)`, dał dokładnie jedną fizyczną relację (`SELECT count(*) FROM relation = 1`) oraz jedną relację w `open_nodes(Y)`. Test jednostkowy sprawdza jedynie zniknięcie source [`graph.rs:2413-2439`](../../crates/memory-core/src/graph.rs#L2413-L2439), a E2E merge nie ma relacji [`tests/e2e.rs:281-320`](../../tests/e2e.rs#L281-L320); żaden nie łapie tego dokładnego regresu. Ręcznie wykonane testy: `cargo test -p memory-core graph::tests::test_merge_entities -- --exact` i `cargo test --test e2e e2e_upsert_merge_and_wipe -- --exact` przeszły.

### Pozostałe ryzyko i rekomendacja

Tabela `relation` nie ma `UNIQUE(from_id, to_id, type_id)` [`graph.rs:466-477`](../../crates/memory-core/src/graph.rs#L466-L477). Kod powstrzymuje duplikaty na obecnej ścieżce zapisu, ale nie daje twardej inwarianty po imporcie, ręcznej naprawie SQLite ani zapisie omijającym usługę mutacji.

W osobnej migracji: (1) zduplikowane rzędy zredukować do jednego deterministycznie (np. najstarszy `created_us`), (2) przeliczyć `graph_stat.relations`, `type_dict.count` oraz stopnie, (3) założyć unikalny indeks, (4) zostawić `ON CONFLICT DO NOTHING`/obecny idempotentny insert. Przed migracją należy wykonać i zapisać odczytowy preflight `GROUP BY from_id,to_id,type_id HAVING count(*) > 1`; istniejące duplikaty inaczej zatrzymają utworzenie indeksu. Dodać test merge z kolizją oraz test migracji ze starym duplikatem.

## 4. Atomowość `merge_entities`

### Dowód i repro

Wszystkie mutacje, w tym `MergeEntities`, wywołują `MutationService::apply_inner`. Ten najpierw pobiera writer, rozpoczyna `BEGIN IMMEDIATE` [`mutation.rs:231-239`](../../crates/memory-core/src/mutation.rs#L231-L239), a commit następuje dopiero po wykonaniu mutacji, licznikach i zapisie zdarzeń [`mutation.rs:262-294`](../../crates/memory-core/src/mutation.rs#L262-L294). `TxGuard::Drop` wydaje `ROLLBACK`, jeżeli commit nie nastąpił [`graph.rs:298-326`](../../crates/memory-core/src/graph.rs#L298-L326).

Wymuszone repro: po utworzeniu `P→X` dodałem na tymczasowej bazie trigger `BEFORE DELETE ON entity WHEN OLD.name='X' RAISE(ABORT, ...)`, po czym wywołałem merge `X→Y`. MCP zwrócił `IO error: forced merge failure`; po błędzie baza nadal zawierała `X,Y,P`, wyłącznie `P→X:pracuje`, a `Y` tylko obserwację `from-Y`. To wyklucza alternatywną hipotezę, że atomowy jest tylko delete, a wcześniejsze dodanie obserwacji/relacji już się utrwaliło.

### Rekomendacja

Brak poprawki funkcjonalnej. Dodać trwały test regresji z wymuszonym błędem w późnym kroku merge’a (najlepiej przez testowy failpoint, nie trigger SQL) i asercjami na obserwacje, relacje, source i outbox. Nie wymaga migracji.

## 5. Pochodzenie obserwacji po merge

### Dowód

Schema obserwacji ma tylko `id`, `entity_id`, `idx`, `body`, `created_us` [`graph.rs:455-464`](../../crates/memory-core/src/graph.rs#L455-L464). Merge kopiuje ciała source bez dodatkowego metadatum [`mutation.rs:689-704`](../../crates/memory-core/src/mutation.rs#L689-L704). W repro `X:[from-X]`, `Y:[from-Y]` po merge zapisane w `Y` były kolejno `from-Y,from-X`; `PRAGMA table_info(observation)` potwierdziło brak kolumny źródła. `created_us` odpowiada tylko na pytanie kiedy rekord zapisano, nie z której encji pochodził.

### Precyzyjna poprawka i migracja

Wprowadzić nieusuwalne pochodzenie w `observation`, np. `origin_entity_id INTEGER NULL` i `origin_entity_name TEXT NULL` (bez FK, bo source jest kasowany). Przy create/upsert/add wypełniać nimi bieżącą encję; przy merge kopię source oznaczać identyfikatorem i nazwą source, natomiast oryginalne obserwacje target zachowują własne pochodzenie. Do modelu publicznego, eksportu i `describe/open` dodać pole jako opcjonalne, aby nie udawać, że stare dane są precyzyjne.

To jest zmiana schematu: nowa migracja powinna najpierw dodać nullable kolumny, potem backfillować istniejące wiersze przez aktualny `entity` (`id` i `name`). Wiersze, dla których encja historycznie już nie istnieje, pozostawić jako `NULL` zamiast przypisywać fałszywe źródło. **Uwaga wykonawcza:** obecne `events::migrate` uruchamia się przed bazowym `CREATE TABLE observation` (`graph.rs:431` przed `graph.rs:455`), więc migracji `ALTER TABLE observation` nie wolno dopisać do tej listy bez rozdzielenia faz bootstrapu. Trzeba najpierw utworzyć bazowy schema graphu, potem uruchomić migracje graphu; testować zarówno świeżą, jak i starą bazę. Testy: obserwacje z obu stron merge, ponowny merge, eksport oraz migracja bazy sprzed kolumn.

## Kolejność

1. Test batch delete i test merge-kolizji (brak ryzyka kontraktowego).
2. Twardy unikalny indeks relacji wraz z preflightem i migracją naprawczą.
3. Migracja pochodzenia obserwacji oraz rozszerzenie kontraktu odczytu.
4. Failpoint test atomowości jako ochrona już naprawionego zachowania.
