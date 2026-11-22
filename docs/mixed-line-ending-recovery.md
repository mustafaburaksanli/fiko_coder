# Karışık LF/CRLF kaynaklarda kontrollü patch toparlanması

Varsayılan eşleşme `exact` olarak kalır. `old_text` dosyada tam bir satır/blok
olarak ve yalnız bir kez bulunmalıdır. Noktalama, kod, girinti politikası ve
satır/blok sınırları korunur. JSON'un çözdüğü gerçek LF ve CRLF baytları
birbirinden farklıdır; modelin “aynı kod” demesi eşleşme kanıtı değildir.

`read_file` ham kaynak metnini ve karışık satır sonlarını korur. HTTP JSON
taşınması bu baytları değiştirmez. Ollama `arguments` alanını JSON metni olarak
gönderirse Rust bu metni normal JSON kurallarıyla çözer. JSON çözüldükten sonra
literal `\n` karakterleri ikinci kez satır sonuna çevrilmez.

`read_lines` ise numaralandırılmış, sınırlı bir görüntüdür. `[READ_LINES]` JSON
başlığındaki aralık/toplam satır bilgisi kaynak kod değildir; görüntünün LF
satır ayırıcıları da dosyanın fiziksel EOL biçimini göstermez. Satır numaraları
ve metadata `old_text` içine kopyalanmamalıdır.

## Strict hata ve açık opt-in

Strict eşleşme başarısız, yalnız CRLF→LF karşılaştırmasında tam ve benzersiz
bir blok bulunmuşsa Rust `PATCH_MATCH` hatasında
`reason="line_endings_mismatch"` bildirir. Hatanın `recovery` alanı:

- `match_mode="line_endings"`,
- gerçek kaynak snapshot'ının `expected_sha256` değeri,
- gerçek dosyadaki başlangıç ve bitişi gösteren `byte_range` (bitiş hariç),
- bu gerçek aralığın `range_sha256` değerini

taşır. Bu metadata adayı uygulamaz ve doğrulama başarısı değildir.

Model, yalnız bu davranışı açıkça seçerek adayı yeniden gönderebilir:

```json
{
  "path": "src/math.rs",
  "old_text": "pub fn quadruple(x: i32) -> i32 {\n    x * 4\n}\n",
  "new_text": "pub fn quadruple(x: i32) -> i32 {\n    x * 4\n}\npub fn quintuple(x: i32) -> i32 { x * 5 }\n",
  "mode": "check",
  "match_mode": "line_endings",
  "expected_sha256": "<Rust recovery.expected_sha256 değerini buraya kopyalayın>"
}
```

Örnekteki hash yer tutucudur; gerçek araç çağrısında Rust'ın gözlediği snapshot'a
ait 64 hexadecimal karakter kullanılmalıdır. Kullanılan kaynak değişmişse
`field="expected_sha256", reason="source_changed"` ile aday reddedilir.
Hash modelin tahmininden veya doğal dildeki başarı iddiasından türetilmez.

`line_endings` yalnız CRLF/LF farkına izin verir. Boşluk düzeltme, fuzzy
eşleşme, noktalama tamamlama, sembol arama veya eksik kod tahmini yapmaz.
Normalize karşılaştırmada birden fazla aynı blok bulunursa aday belirsizdir
ve reddedilir. Aynı kodun farklı EOL biçimindeki ikinci kopyası da bu sayıya
dahildir. Bare CR, LF/CRLF ile aynı kabul edilmez.

Rust eşleşen normalize konumu gerçek kaynak bayt aralığına taşır. Dosyanın
tamamı normalize edilmez. Eşleşen aralığın dışındaki prefix/suffix baytları
aynen kalır; `new_text` ise modelin gönderdiği biçimde kopyalanır. Kaynağın
eski EOL biçimi `new_text` üzerine otomatik uygulanmaz.

## Doğrulama, kilit ve izinler

Opt-in bir yazma yetkisi değildir. Yalnız açık Modify kapsamı içindeki hedefler
kullanılabilir; korunan bir dosya bu seçenekle değiştirilemez. Geçici Cargo
çalıştırma izni, adayın geçici kopyada doğrulanması, Verified Patch Lock,
proje fingerprint'i ve açık uygulama onayı gereklilikleri korunur.

`apply_patch`, güncel dosyadan hazırlanan adayın gerçek original/updated içerik
özetlerini ve proje fingerprint'ini doğrulama kaydıyla karşılaştırır. Opt-in
seçilmişse beklenen kaynak SHA da yeniden denetlenir. Ham eşleşme seçenekleri
JSON'unun birebir aynılığına dayanan ayrı bir kilit yoktur. Kaynak değişirse
eski doğrulama/onay güncel dosyaya taşınmaz. Cargo derleme hatası gerçek dosyaya
uygulama kanıtı değildir. Son
doğrulama uygulamadan sonra başarısız olursa mevcut rollback akışı, karışık
EOL dahil orijinal baytları geri yüklemeye çalışır; başarısı Rust tarafından
bildirilir.

Strict hatayı aldıktan sonra aynı LF `old_text` ile açık `line_endings` ve doğru
hash'i göndermek kontrollü bir strateji değişikliğidir. Yalnız `new_text`, yol
yazımı veya Cargo mode değiştirmek aynı eski-metin hatasını düzeltmez.

## Regresyon kapsamı ve sınırlar

`src/mixed_line_ending_tests.rs`, kontrollü HTTP yanıtlarıyla şu davranışları
sınar: JSON transportunda ham kaynak ve LF argümanlarının korunması; numaralı
okumada fiziksel EOL bilgisinin kaybı; strict hata→hash'li opt-in→gerçek Cargo
doğrulaması→kullanıcı onayı→uygulama; normalize çift blok/noktalama/eski hash
reddi; gerçek derleme hatası; son test hatasında birebir rollback; korunan
dosyada opt-in yasağı; başarılı doğrulama sonrasındaki dış değişikliğin onaydan
önce reddi; strict apply hatasının aynı old/new ile opt-in toparlanmasını
kilitlememesi. Fixture projelerinin Cargo target dizinleri ayrıdır.

Bu testler canlı Ollama veya Qwen kullanmaz. Kontrollü model yanıtlarıyla
kanıtlanan toparlanma, Qwen'in aynı çağrıyı canlı kullanımda kendiliğinden
üreteceği garantisini vermez. Canlı model kabulü ayrıca gözlenmelidir.
