//! As 80 classes do COCO, na ordem em que os modelos YOLO as numeram.
//!
//! O nome em inglês é o identificador estável (é o que vai para o
//! `devices.toml`); o nome em português é só para a interface.

/// `(nome em inglês, nome em português)`, indexado pelo id da classe.
pub const COCO: [(&str, &str); 80] = [
    ("person", "pessoa"),
    ("bicycle", "bicicleta"),
    ("car", "carro"),
    ("motorcycle", "moto"),
    ("airplane", "avião"),
    ("bus", "ônibus"),
    ("train", "trem"),
    ("truck", "caminhão"),
    ("boat", "barco"),
    ("traffic light", "semáforo"),
    ("fire hydrant", "hidrante"),
    ("stop sign", "placa de pare"),
    ("parking meter", "parquímetro"),
    ("bench", "banco"),
    ("bird", "pássaro"),
    ("cat", "gato"),
    ("dog", "cachorro"),
    ("horse", "cavalo"),
    ("sheep", "ovelha"),
    ("cow", "vaca"),
    ("elephant", "elefante"),
    ("bear", "urso"),
    ("zebra", "zebra"),
    ("giraffe", "girafa"),
    ("backpack", "mochila"),
    ("umbrella", "guarda-chuva"),
    ("handbag", "bolsa"),
    ("tie", "gravata"),
    ("suitcase", "mala"),
    ("frisbee", "frisbee"),
    ("skis", "esquis"),
    ("snowboard", "snowboard"),
    ("sports ball", "bola"),
    ("kite", "pipa"),
    ("baseball bat", "taco de beisebol"),
    ("baseball glove", "luva de beisebol"),
    ("skateboard", "skate"),
    ("surfboard", "prancha de surfe"),
    ("tennis racket", "raquete de tênis"),
    ("bottle", "garrafa"),
    ("wine glass", "taça"),
    ("cup", "copo"),
    ("fork", "garfo"),
    ("knife", "faca"),
    ("spoon", "colher"),
    ("bowl", "tigela"),
    ("banana", "banana"),
    ("apple", "maçã"),
    ("sandwich", "sanduíche"),
    ("orange", "laranja"),
    ("broccoli", "brócolis"),
    ("carrot", "cenoura"),
    ("hot dog", "cachorro-quente"),
    ("pizza", "pizza"),
    ("donut", "rosquinha"),
    ("cake", "bolo"),
    ("chair", "cadeira"),
    ("couch", "sofá"),
    ("potted plant", "planta em vaso"),
    ("bed", "cama"),
    ("dining table", "mesa de jantar"),
    ("toilet", "vaso sanitário"),
    ("tv", "televisão"),
    ("laptop", "notebook"),
    ("mouse", "mouse"),
    ("remote", "controle remoto"),
    ("keyboard", "teclado"),
    ("cell phone", "celular"),
    ("microwave", "micro-ondas"),
    ("oven", "forno"),
    ("toaster", "torradeira"),
    ("sink", "pia"),
    ("refrigerator", "geladeira"),
    ("book", "livro"),
    ("clock", "relógio"),
    ("vase", "vaso"),
    ("scissors", "tesoura"),
    ("teddy bear", "urso de pelúcia"),
    ("hair drier", "secador de cabelo"),
    ("toothbrush", "escova de dentes"),
];

pub const COUNT: usize = COCO.len();

/// Classes marcadas ao ligar a detecção pela primeira vez: o que costuma
/// interessar numa câmera de segurança.
pub const DEFAULT: &[&str] = &[
    "person",
    "bicycle",
    "car",
    "motorcycle",
    "bus",
    "truck",
    "cat",
    "dog",
];

/// Classes que a interface lista no topo, antes das demais em ordem alfabética.
pub const COMMON: &[&str] = &[
    "person",
    "cat",
    "dog",
    "car",
    "motorcycle",
    "bicycle",
    "bus",
    "truck",
    "bird",
    "horse",
    "backpack",
    "suitcase",
];

/// Id da classe a partir do nome em inglês.
pub fn id_of(name: &str) -> Option<usize> {
    COCO.iter().position(|(en, _)| *en == name)
}

/// Nome em português da classe (ou `"?"` para um id fora da faixa).
pub fn pt(id: usize) -> &'static str {
    COCO.get(id).map_or("?", |(_, pt)| pt)
}

/// Nome em inglês, o identificador gravado no cadastro.
pub fn en(id: usize) -> &'static str {
    COCO.get(id).map_or("?", |(en, _)| en)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_seguem_a_ordem_do_coco() {
        assert_eq!(id_of("person"), Some(0));
        assert_eq!(id_of("car"), Some(2));
        assert_eq!(id_of("dog"), Some(16));
        assert_eq!(id_of("toothbrush"), Some(79));
        assert_eq!(id_of("unicórnio"), None);
    }

    #[test]
    fn listas_padrao_so_tem_classes_validas() {
        for name in DEFAULT.iter().chain(COMMON) {
            assert!(id_of(name).is_some(), "classe desconhecida: {name}");
        }
    }

    #[test]
    fn nomes_em_ingles_sao_unicos() {
        let mut names: Vec<_> = COCO.iter().map(|(en, _)| *en).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), COUNT);
    }
}
