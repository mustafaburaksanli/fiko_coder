# PostgreSQL kalıcı bellek — v1.2.1

Bu kılavuz Windows PowerShell ve PostgreSQL 17 içindir. Bellek isteğe bağlıdır:
`FIKO_DATABASE_URL` yoksa FIKO önceki stateless akışla çalışır. Derleme ve normal
birim testleri için PostgreSQL veya canlı Ollama gerekmez. Aşağıdaki canlı kabul
adımları çalıştırılacak senaryolardır; uygulanmış test sonucu anlamına gelmez.

v1.2.1 bakım komutları `fiko_coder.exe --memory-migrate` ve
`fiko_coder.exe --memory-check` biçimindedir; ikisi de açıkça ayarlanmış
`FIKO_DATABASE_URL` kullanır. Normal `fiko_coder.exe` başlangıcı migration
çalıştırmaz.

## Kurulum ve yetkiler

PostgreSQL 17 sunucusunu ve `psql` istemcisini [resmî Windows indirme
sayfasından](https://www.postgresql.org/download/windows/) kurun. Kurulumdan
sonra `psql --version` ile istemciyi, `SELECT version();` ile bağlandığınız
sunucuyu kontrol edin. `psql` PATH'te değilse örneğin
`C:\Program Files\PostgreSQL\17\bin\psql.exe` tam yolunu kullanın.

Aşağıdaki işlemler yeni, FIKO'ya ayrılmış `fiko_memory` veritabanı içindir.
Var olan uygulama tablolarını veya veritabanlarını silme/yeniden oluşturma adımı
yoktur. İlk kurulumda PostgreSQL yöneticisiyle interaktif oturum açın:

```powershell
psql -h 127.0.0.1 -p 5432 -U postgres -d postgres -W
```

`psql` içinde rolleri oluşturun. `\password` parolayı interaktif sorar;
gerçek parolaları SQL dosyasına veya PowerShell geçmişine yazmayın.

```sql
CREATE ROLE fiko_migrator LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE;
\password fiko_migrator
CREATE ROLE fiko_app LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE;
\password fiko_app
CREATE DATABASE fiko_memory OWNER fiko_migrator;
REVOKE ALL ON DATABASE fiko_memory FROM PUBLIC;
GRANT CONNECT ON DATABASE fiko_memory TO fiko_app;
\connect fiko_memory
REVOKE CREATE ON SCHEMA public FROM PUBLIC;
\quit
```

Migration rolü DDL için, uygulama rolü yalnız çalışma zamanı içindir. Roller
birbirine üye yapılmaz. [PostgreSQL GRANT
belgesi](https://www.postgresql.org/docs/17/sql-grant.html) tablo, şema ve
sequence izinlerini ayrı ele alır.

## Bağlantıyı sırları yazdırmadan ayarlama

Bu yardımcı yalnız mevcut PowerShell prosesinin ortam değişkenini ayarlar.
Parola URI'ye uygun kaçışla aktarılır; bağlantı dizesini konsola dökmez.
Ortam değişkeni parola içerir; onu terminal çıktısına, hata raporuna veya Git'e
kopyalamayın. [psql parola
komutunun](https://www.postgresql.org/docs/17/app-psql.html) etkileşimli
kullanımı da bağlantı URL'sini komut satırına yazmayı gerektirmez.

```powershell
function Set-FikoDatabaseUrl {
    param(
        [ValidateSet('FIKO_DATABASE_URL', 'FIKO_TEST_DATABASE_URL', 'FIKO_TEST_UPGRADE_DATABASE_URL')]
        [string] $Name = 'FIKO_DATABASE_URL',
        [string] $User = 'fiko_app',
        [string] $Database = 'fiko_memory',
        [int] $Port = 5432
    )
    $credential = Get-Credential -UserName $User -Message 'PostgreSQL parolası'
    $encodedUser = [Uri]::EscapeDataString($credential.UserName)
    $encodedPassword = [Uri]::EscapeDataString($credential.GetNetworkCredential().Password)
    $encodedDatabase = [Uri]::EscapeDataString($Database)
    $connection = 'postgresql://{0}:{1}@127.0.0.1:{2}/{3}' -f $encodedUser, $encodedPassword, $Port, $encodedDatabase
    [Environment]::SetEnvironmentVariable($Name, $connection, 'Process')
    $connection = $null
    $encodedPassword = $null
    $credential = $null
}

# FIKO deposundayken, build tamamlandıktan sonra exe yolunu kaydedin.
$FikoExe = (Resolve-Path -LiteralPath '.\target\release\fiko_coder.exe').Path
Set-FikoDatabaseUrl -User fiko_migrator
& $FikoExe --memory-migrate
if ($LASTEXITCODE -ne 0) { throw 'Migration tamamlanmadı; uygulama rolüne geçmeden hatayı inceleyin.' }
```

Migration yalnız açık `--memory-migrate` komutuyla çalışır. Normal başlangıç
otomatik DDL yapmaz. İlk kurulum `fiko_memory` şemasını ve sürüm/checksum
kaydını tek transaction içinde oluşturur. Aynı migration tekrar çalıştırılabilir;
uyuşmayan şema veya checksum sessizce sıfırlanmaz. Uygulamanın başka şemalarına
DDL uygulanmaz.

### Mevcut v1.2 belleğini yükseltme

v1.2.1, değiştirilmemiş `0001_memory.sql` üzerine
`0002_provenance.sql` migration'ını ekler. Migration 2 yalnız FIKO'nun
`task_history` tablosuna nullable `modified_paths` alanını ve boyut kontrolünü
ekler; eski görevleri silmez veya hedef listesinden değişiklik kanıtı uydurarak
backfill yapmaz. Sürüm tablosunda migration 1 ve 2'nin ayrı checksum'ları tutulur.
Sadece v1 şemasıyla normal bağlantı açılması açık migration yönergesiyle
reddedilir. Güncel executable ile migration rolünü kullanarak yükseltin:

```powershell
Set-FikoDatabaseUrl -User fiko_migrator
& $FikoExe --memory-migrate
if ($LASTEXITCODE -ne 0) { throw 'Bellek şeması yükseltilemedi; mevcut kayıtlar sıfırlanmaz.' }
Set-FikoDatabaseUrl -User fiko_app
& $FikoExe --memory-check
if ($LASTEXITCODE -ne 0) { throw 'Yükseltme sonrası bağlantı/şema kontrolü başarısız.' }
```

Aynı yükseltme komutu tekrar çalıştırılabilir. Beklenmeyen sürüm veya checksum
çatışması varsa migration durur; uygulama verisini temizlemez. Migration rolü
bu tabloyu oluşturan/sahip olan rol olmalıdır; normal uygulama hesabına DDL
yetkisi verilmez.

Migration tamamlandıktan sonra yönetici veya `fiko_migrator` hesabıyla
`fiko_memory` veritabanına bağlanıp çalışma zamanı izinlerini verin:

```sql
GRANT USAGE ON SCHEMA fiko_memory TO fiko_app;
GRANT SELECT ON fiko_memory.schema_version TO fiko_app;
GRANT SELECT, INSERT, UPDATE ON
    fiko_memory.projects,
    fiko_memory.project_files,
    fiko_memory.task_history,
    fiko_memory.task_files
TO fiko_app;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA fiko_memory TO fiko_app;
```

Uygulama hesabına `CREATE`, `DELETE`, `TRUNCATE` veya migration rolü üyeliği
verilmez. FIKO'nun kayıt işlemleri `SELECT`, `INSERT`, `UPDATE` kullanır.

```powershell
Set-FikoDatabaseUrl -User fiko_app
& $FikoExe --memory-check
if ($LASTEXITCODE -ne 0) { throw 'Bellek bağlantısı veya şema kontrolü başarısız.' }

# İsteğe bağlı; varsayılanlar 4 kayıt ve 4096 bayttır.
$env:FIKO_MEMORY_MAX_RECORDS = '4'
$env:FIKO_MEMORY_MAX_BYTES = '4096'
```

Bağlam bütçesinin üst sınırları 16 kayıt ve 16384 bayttır. Bunlar dosya okuma
limitlerinin yerine geçmez. Bağlantı kesilirse bellek devre dışı kalır; önceki
intent, araç izinleri, doğrulama ve kullanıcı onayı kurallarıyla görev devam
edebilir. Bellek sorunu görev başarısı olarak sunulmaz.

Havuz en fazla iki bağlantı açar; bağlantı/edinme sınırı 3 saniye,
sunucu sorgu sınırı 3 saniye ve kilit bekleme sınırı 1,5 saniyedir. CLI'nin
migration/bağlantı kontrolü ve agent'ın bağlanma/görev başlatma/sonuç kaydetme
adımlarının her biri Rust tarafında ayrıca 5 saniyeyle sınırlıdır. Sunucudan
yanıt gelmezse kalıcı hafıza hatası bildirilir; kodlama akışı bekletilmez.

## Kaydedilen bilgi ve güven sınırı

| Kayıt | İçerik |
| --- | --- |
| `projects` | Kanonik proje kökünden SHA-256 kimliği, kök görünümü, proje türü |
| `project_files` | Projeye göreli yol, güncel gözlem hash'i, boyut, sınırlı sembol ve ilişkili yol bilgisi |
| `task_history` | Rust intent'i, kontrollü konu özeti, hedef yollar, kanıtlı değiştirilmiş yollar, gerçek aşama/sonuç, komut durumları ve exit code'ları |
| `task_files` | Göreve özgü snapshot ve observed/target rolü; sonraki görev önceki hash'i değiştirmez |
| `schema_version` | Migration sürümü ve checksum |

Kaynak kodun tamamı, terminal çıktısının tamamı, modelin serbest raporu ve
ham kullanıcı mesajı belleğe kopyalanmaz. Özet, kontrollü konu sözlüğü ve güvenli
göreli yollar üzerinden oluşturulur. Tablo verisi talimat sayılmaz. Prompt'a
alınan kayıtlar proje kimliği, konu/yol ilgisi ve diskteki güncel dosya hash'iyle
tekrar kontrol edilir. Uyuşmayan snapshot eski kabul edilir ve bağlamdan çıkar.
İlgili kayıtlarda bile DB'deki serbest `summary` metni prompt'a taşınmaz.
Salt okunmuş bir dosyanın geçmişi de göreve bağlı snapshot yolu üzerinden
bulunabilir; güvenli dosya adı (`lib.rs`) eşleşmesi desteklenir. Hiç ilgili konu
veya yol yoksa yalnız intent aynı diye yakın geçmiş prompt'a eklenmez.

### Dosya rolü ve gerçek değişiklik kanıtı

`observed` dosyanın görevde gözlendiğini, `target` değişiklik hedefi olarak
planlandığını belirtir. Hedef olmak dosyaya yazıldığının kanıtı değildir.
`modified_paths` yalnız Rust'ın gerçek uygulama kaydından üretilen ayrı bir
alandır. Boş liste, görevin hiçbir dosyayı değiştirmediğinin bilindiğini;
`NULL`, eski kayıtta bu bilginin bulunmadığını belirtir. Legacy `NULL`,
`true` veya boş listeye çevrilmez.

Bağlamda `historical_task.files[].modified_in_task` bu kanıttan `true`,
`false` veya `null` olarak üretilir. Örneğin yalnız `src/math.rs` değiştirilip
`src/lib.rs` okunmuşsa math için target/true, lib için observed/false taşınır.
Uygulama reddedilmiş bir target için sonuç false olur. Rollback sonrasında
`modified_in_task=true` geçmişte yazma olduğunu anlatabilir; dosyanın bugün
etkin değişiklik olarak kaldığını anlatmaz. `active_change`, değişiklik olmadığı
veya geri alındığı bilindiğinde false, güncel etkinlik kanıtlanamadığında null
olur; sadece geçmiş başarı veya hash eşleşmesiyle true ilan edilmez.

Geçmiş ve güncel gözlem JSON'da ayrıdır: `historical_task` DB kökenli geçmiş
iddiasını, `current_disk` Rust'ın yeniden okuduğu dosya metadata'sını taşır.
`hash_matches_historical=true` yalnız dosya içeriğinin kayıtlı hash ile aynı
olduğunu belirtir. Bu sorguda Cargo çalıştırılmaz;
`current_disk.cargo_verification="not_rerun"` olarak belirtilir. Hash eşleşmesi
yeniden derleme, test başarısı, aktif patch veya yeni Verified Patch Lock kanıtı
değildir.

`verified` uygulanıp son doğrulaması başarılı olmuş görevi, `not_applied`
uygulanmamış görevi, `rolled_back` uygulanıp geri alınmış görevi, `uncertain`
son durumu kesinleştirilemeyen işlemi gösterir. `applied=true, rolled_back=true`
geçmişte uygulama yapıldığını ve geri alındığını belirtir; değişikliğin hâlâ
etkin olduğu anlamına gelmez. Modelin “başarılı” sözü bu alanları belirlemez.

Bir görevde en fazla 16 komutun süreç metadata'sı kaydedilir. Mevcut bazı
`Err(String)` hata yolları ayrıntılı süreç kaydı taşımadığından ilgili komutun
metadata'sı DB'de bulunmayabilir; bu geçmiş tüm olası süreç olaylarının eksiksiz
günlüğü değildir. Eksik komut, eksik exit code veya çalıştırılmamış doğrulama
başarı sayılmaz. Kayıtlı süreç sonucu yalnız Rust'ın gerçekten gözlediği komut
metadata'sından gelir; hata metninden yeni exit code veya başarı üretilmez.

Analyze/Explain yanıtında model metni
`[MODEL ACIKLAMASI - DOGRULANMAMIS]` etiketiyle gösterilir. Ardından gelen
`[RUST KANIT OZETI]`, kaynağı `this_task_rust_dispatcher` olan olgusal JSON
raporudur: bu görevde gerçekten okunan dosyalar, `modified_paths` ve
`commands_run_this_task` ayrı alanlardır. Okuma kanıtı yalnız başarılı
`read_file`, `read_lines` veya `search_in_file` araçlarının gerçek host
çıktısından gelir; modelin taklit ettiği araç yanıtı kanıt oluşturmaz.

Özet en fazla 8 okumayı ve okuma başına en fazla 2 adet 180 karakterlik gerçek
kaynak alıntısını taşır. Bu sınırlı alıntılar tüm dosyayı kapsamaz; bir sembolün
yokluğunu veya AST düzeyinde doğruluğu kanıtlamaz. Mevcut Rust süreç raporu da
korunur. Güncel görev kanıtı, yukarıdaki DB kökenli `historical_task` bilgisinden
ayrıdır; eski komut bugün çalıştırılmış gibi gösterilmez.

Modelin serbest metni Rust özetini, kayıtlı süreç sonucunu veya uygulanmış dosya
listesini değiştirmez. Yerel model yine de açıklamasında yanlış çıkarım
yapabilir; bu etiketleme modelin her doğal dil cümlesinin doğru olduğu veya
tüm çelişkilerin otomatik saptandığı garantisini vermez. Canlı kabulde Rust
özeti, araç izi, gerçek dosyalar ve süreç sonuçları birlikte esas alınır.

Bellek yeni yazma yetkisi, Verified Patch Lock kanıtı veya kullanıcı onayı
oluşturmaz. Eski bir başarı bugünkü adayın doğrulandığı anlamına gelmez.
SHA-256 güncellik kontrolü, uygulama öncesindeki mevcut proje fingerprint ve
aday kimliği kontrollerinin yerine geçmez. Bellek var diye Analyze, Explain
veya Generate görevleri dosya yazamaz.

Depo otomatik veri silmez. İlk sınırlar 128 proje, proje başına 2048 dosya ve
512 görevdir. Sınır dolduğunda kayıt reddedilir; sessizce eski kayıtların üzerine
yazılmaz. Kayıt saklama/temizleme yöneticiye ait ayrı bir işlemdir.

## Gerçek PostgreSQL regresyon testleri

Normal `cargo test`, canlı PostgreSQL gerektiren testleri `ignored` olarak
gösterir. Bu testleri çalıştırmak için uygulama DB'sinden ayrı, **yerel** bir DB
oluşturun. Örnek yönetici `psql` oturumunda:

```sql
CREATE ROLE fiko_test LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE;
\password fiko_test
CREATE DATABASE fiko_memory_test OWNER fiko_test;
CREATE DATABASE fiko_memory_test_upgrade OWNER fiko_test;
```

Test rolü yalnız bu yeni DB'nin sahibidir; testler migration ve kendilerine ait
kanıt tablosu oluşturacağı için çalışma zamanı `fiko_app` rolü yeterli değildir.
Testler production ayarını kullanmaz. Normal test DB'si ve upgrade DB'si
birbirinden ve uygulama DB'sinden ayrı olmalıdır. DB adı `fiko` ile başlamalı ve `_test`
içermeli, host loopback olmalıdır; sunucunun `current_database()` sonucu da
doğrulanır. Açık çalıştırmada ortam değişkeni yoksa test hata verir.

```powershell
# FIKO deposunda çalıştırın. Uygulama bağlantısını test URL'siyle karıştırmayın.
Remove-Item Env:FIKO_DATABASE_URL -ErrorAction SilentlyContinue
Set-FikoDatabaseUrl -Name FIKO_TEST_DATABASE_URL -User fiko_test -Database fiko_memory_test
Set-FikoDatabaseUrl -Name FIKO_TEST_UPGRADE_DATABASE_URL -User fiko_test -Database fiko_memory_test_upgrade
cargo test --test postgres_memory -- --ignored --test-threads=1
if ($LASTEXITCODE -ne 0) { throw 'Gerçek PostgreSQL testleri başarısız.' }
cargo test --lib postgres_agent_uc_dosyali_onay_hafiza_ret_ve_rollback -- --ignored --test-threads=1
if ($LASTEXITCODE -ne 0) { throw 'Agent–PostgreSQL uçtan uca testi başarısız.' }
Remove-Item Env:FIKO_TEST_DATABASE_URL -ErrorAction SilentlyContinue
Remove-Item Env:FIKO_TEST_UPGRADE_DATABASE_URL -ErrorAction SilentlyContinue
```

Testler kendi benzersiz proje kayıtlarıyla çalışır. DB'de `DELETE`, `TRUNCATE`,
`DROP` veya genel sıfırlama yapmaz; kayıtlar ve tek migration kanıt tablosu
kalır. Tekrarlı çalıştırmalar proje kotasına ulaşabileceğinden uzun süreli test
kullanımında yönetici yeni bir özel test DB'si ayırmalıdır. Testler yalnız
kendilerinin oluşturduğu, sınırı doğrulanmış geçici proje dizinlerini temizler.

Upgrade testi `FIKO_TEST_UPGRADE_DATABASE_URL` veritabanında ilk çalışmada boş
`fiko_memory` şemasına immutable v1 migration'ını ve kendine ait eski kayıtları
kurar, sonra gerçek v1→v2 geçişini çalıştırır. Sonraki çalışmada yalnız aynı
tanınan fixture'ı kabul ederek idempotency'yi sınar. Tanınmayan mevcut şemada
fail eder; DROP, eski sürüme çevirme veya cleanup yapmaz. Eski task/files/commands
verisi, roller ve `modified_paths=NULL` yükseltme boyunca korunmalıdır.

Kapsam: migration tekrarının checksum'u ve başka tabloyu koruması; aynı proje
kimliğinin yeni bağlantıda ve ayrı test prosesinde bulunması; proje izolasyonu;
ret/başarı/rollback ve exit code kayıtları; eski hash snapshot'ı; ilgili geçmiş
seçimi; SQL/prompt injection sınırları; eşzamanlı yazma ve composite FK;
idempotent bitirme ve görev kotası. Bağlantı hatasının URL/parola sızdırmaması
ayrı, canlı DB gerektirmeyen bir testtir. Bunlar canlı Qwen davranışını ölçmez.
Salt gözlenen dosyanın tam yol/dosya adı üzerinden bulunması ve ilgisiz yakın
geçmişe intent üzerinden geri dönülmemesi de gerçek DB üzerinde sınanır.
Ek olarak gerçek `MemoryRuntime` connect/begin/finish döngüsü, üç dosyalı
metadata snapshot'ının diskten yeniden gözlenmesi ve Generate görevinin dosya
snapshot'ı kaydetmemesi sınanır. Bu kayıt testi Cargo doğrulaması çalıştırmaz;
kontrollü süreç metadata'sıyla kayıt sınırını test eder.
v1.2.1 testleri ayrıca target/observed rollerinin kalıcılığını, gerçek yazma
listesini, uygulanmayan hedefin false ve legacy kaydın unknown kalmasını,
iki migration checksum'unu ve eski verinin yükseltmede korunmasını doğrular.

Ek agent testi gerçek PostgreSQL ve Cargo ile kontrollü HTTP model yanıtlarını
birleştirir: üç dosyalı doğrulama, modelin metinsel onay isteği sonrası gerçek
Rust onayı, uygulama, ret ve rollback kayıtları sınanır. İki sonraki Analyze
isteğinde hafızanın yalnız kullanıcı veri mesajı olarak ve tek kopyayla
gönderildiği doğrulanır. Canlı Ollama/Qwen gerektirmez, model çağrı sayısını
artıran ek bir hafıza çağrısı yapılmaz.

## Canlı kabul senaryoları

Önce ayrı bir geçici Cargo projesi hazırlayın. FIKO deposunu veya gerçek
uygulama projesini kabul fixture'ı olarak kullanmayın. `$FikoExe` yukarıda
depo içindeyken çözülmüş mutlak yol olmalıdır.

```powershell
$acceptanceRoot = Join-Path ([IO.Path]::GetTempPath()) ('fiko_memory_acceptance_' + [guid]::NewGuid().ToString('N'))
cargo new --lib --name fiko_memory_acceptance $acceptanceRoot
if ($LASTEXITCODE -ne 0) { throw 'Kabul projesi oluşturulamadı.' }
Set-Content -LiteralPath (Join-Path $acceptanceRoot 'src\math.rs') -Encoding utf8 -Value 'pub fn double(x: i32) -> i32 { x * 2 }'
Set-Content -LiteralPath (Join-Path $acceptanceRoot 'src\lib.rs') -Encoding utf8 -Value 'pub mod math;'
Set-FikoDatabaseUrl -User fiko_app
Set-Location -LiteralPath $acceptanceRoot
& $FikoExe
```

1. **Kalıcı kayıt / yeniden başlatma.** `Modify src/math.rs: preserve double
   and add triple(x: i32) -> i32. Add tests.` isteğini verin. Geçici Cargo izni
   ve gerçek uygulama onayını okuyarak yanıtlayın. Sonuç gerçekten başarılıysa
   `cikis` ile çıkıp aynı dizinde exe'yi tekrar açın. Aynı konudaki bir Analyze
   isteğinde ilgili geçmiş güncel dosya hash'iyle kabul edilmeli; yeni proses
   belleği önceki prosesin RAM'inden okuyamaz.
2. **Eski bağlam.** Önceki başarılı dosyayı editörle dışarıdan değiştirin.
   Yeniden açıp aynı konu hakkında sorun. Önceki snapshot güncel olarak
   sunulmamalı; eski kayıt sayısı/uyarısı ile güncellik reddi izlenebilmelidir.
3. **Ret ve rollback geçmişi.** Bir Modify isteğinde gerçek uygulama onayını
   reddedin; `not_applied` kaydı oluşmalıdır. Başka bir kabul görevinde son
   doğrulama nedeniyle otomatik rollback oluşursa `rolled_back` ve komut exit
   code'ları kaydedilmelidir. “Rollback mekanizmasını incele” gibi ilgili
   istekte bu geçmiş eski başarı gibi kullanılmamalıdır. Agent görevi dışındaki
   elle `geri al` işlemi şu anda ayrı bir kalıcı görev/olay kaydı oluşturmaz.
   Sonraki ilgili bellek sorgusunda disk yeniden hash'lenir; geri alma sonrası
   dosyayla hash'i uyuşmayan eski snapshot bağlama alınmaz. Bu güncellik
   kontrolü ayrı bir rollback geçmiş kaydının var olduğu anlamına gelmez.
4. **Üç dosyalı uygulama.** `Modify src/math.rs: preserve double and add triple;
   create src/format.rs with a formatting function; register format in
   src/lib.rs; add appropriate tests. Do not modify files outside this
   project.` isteğini verin. Rust'ın üç dosyalı planı, birleşik diff'i, geçici
   doğrulaması, tek açık uygulama onayı ve son Cargo sonuçları gözlenmelidir.
   Bellekteki eski plan yeni uygulama onayı yerine geçmemelidir.
5. **Son doğrulama hatası ve rollback.** Ayrı bir fixture görevinde derlenen
   ama kasten başarısız test içeren aday hazırlatın. Geçici `cargo check`
   başarılı olsa bile onay sonrası `cargo test` exit code 101 üretirse gerçek
   dosyaların geri alındığını, görevin başarılı sayılmadığını ve DB kaydında
   `rolled_back`, `applied=true`, `rolled_back=true`, komut exit code 101
   bulunduğunu denetleyin. Model bu adayı üretmezse senaryo çalışmış sayılmaz;
   deterministik otomatik regresyonlar kontrollü yanıt/fixture kullanır.
6. **DB çevrimdışı.** Yeni PowerShell prosesinde, gerçek bağlantı yerine
   `postgresql://offline:unused@127.0.0.1:1/fiko_memory` gibi sahte yerel adres
   kullanın. `--memory-check` hata koduyla dönmeli; normal FIKO görevi ise
   bellek kullanılamadığını açıkça bildirerek stateless devam edebilmelidir.
   Araç, uygulama onayı ve Rust süreç sonucu korumaları korunmalıdır.

Son kayıtları bağlantı URL'sini yazdırmadan interaktif `psql` ile inceleyin:

```powershell
psql -h 127.0.0.1 -p 5432 -U fiko_app -d fiko_memory -W
```

```sql
SELECT id, kind, root_display FROM fiko_memory.projects ORDER BY id DESC LIMIT 5;
SELECT id, project_id, phase, outcome, applied, rolled_back, modified_paths, commands
FROM fiko_memory.task_history ORDER BY id DESC LIMIT 10;
```

Gerçek kabul kanıtında görev isteğini, Rust araç izini, kullanıcı onayı
sonucunu, süreç durumlarını/exit code'larını ve son dosya durumunu birlikte
değerlendirin. Modelin belleği “hatırladım” demesi tek başına kalıcılık,
doğrulama veya doğru dosya seçimi kanıtı değildir.
