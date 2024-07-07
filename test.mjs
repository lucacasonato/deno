import crypto from 'node:crypto';

const { privateKey, publicKey } = crypto.generateKeyPairSync('rsa', { 
  modulusLength: 2048,
});

console.log('Private key:', privateKey.export({ format: "pem", type: "pkcs1" }));
console.log('Public key:', publicKey.export({ format: "pem", type: "pkcs1" }));

// const sign = crypto.createSign('sha512');
// sign.update('some data to sign');
// sign.end();
// const signature = sign.sign(privateKey);
// console.log('Signature:', signature.toString('hex'));

// const verify = crypto.createVerify('sha512');
// verify.update('some data to sign');
// verify.end();
// console.log('Verify:', verify.verify(publicKey, signature));

